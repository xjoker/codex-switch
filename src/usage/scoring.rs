use crate::auth;

use super::{
    Candidate, FREE_FLOOR_PCT, MIN_WARMUP_ELAPSED_SECS, ScoredCandidate, UsageInfo, WINDOW_5H_SECS,
    WINDOW_7D_SECS, WindowUsage,
};

/// Returns true only when usage data proves a warmup-opened window is active.
pub fn warmup_window_active(w: &WindowUsage, window_secs: i64, now: i64) -> bool {
    let resets_at = match w.resets_at {
        Some(t) if t > now => t,
        _ => return false,
    };
    if w.used_percent.unwrap_or(0.0) <= 0.0 {
        return false;
    }
    let elapsed = window_secs - (resets_at - now);
    elapsed >= MIN_WARMUP_ELAPSED_SECS
}

/// Decide whether warmup should be skipped because the relevant window is already active.
///
/// Paid accounts have both 5h (primary) and 7d (secondary) windows; only the 5h window
/// is what warmup is meant to (re)open, so a still-active 7d window must NOT suppress
/// warmup once the 5h window has closed. Free accounts only have the 7d window, so it
/// is the only signal available.
pub fn usage_has_active_warmup_window(u: &UsageInfo, now: i64) -> bool {
    let main_active = match u.primary.as_ref() {
        Some(w) => warmup_window_active(w, WINDOW_5H_SECS, now),
        None => u
            .secondary
            .as_ref()
            .is_some_and(|w| warmup_window_active(w, WINDOW_7D_SECS, now)),
    };
    let additional_active = u
        .additional_limits
        .iter()
        .filter(|limit| {
            limit
                .metered_feature
                .as_deref()
                .is_some_and(|feature| feature.starts_with("codex_"))
        })
        .all(|limit| {
            if limit.allowed == Some(false) || limit.limit_reached == Some(true) {
                return true;
            }
            match limit.primary.as_ref() {
                Some(w) => warmup_window_active(w, WINDOW_5H_SECS, now),
                None => limit
                    .secondary
                    .as_ref()
                    .is_some_and(|w| warmup_window_active(w, WINDOW_7D_SECS, now)),
            }
        });
    main_active && additional_active
}

/// Calculate pace: the expected used_percent if consumption were even across the window.
/// Returns None if resets_at is unavailable.
pub fn pace_percent(w: &WindowUsage, window_secs: i64) -> Option<f64> {
    let resets_at = w.resets_at?;
    let now = auth::now_unix_secs();
    let remaining_secs = (resets_at - now).max(0) as f64;
    let elapsed_secs = (window_secs as f64 - remaining_secs).clamp(0.0, window_secs as f64);
    Some((elapsed_secs / window_secs as f64 * 100.0).clamp(0.0, 100.0))
}

/// Pace marker for UI/text rendering.
/// Hide it when the UI would already render `0% left`, because there is no meaningful
/// remaining quota to pace against.
pub fn visible_pace_percent(w: &WindowUsage, window_secs: i64) -> Option<f64> {
    let used = w.used_percent.unwrap_or(0.0).min(100.0);
    let remaining = (100.0 - used).max(0.0);
    if remaining.round() <= 0.0 {
        None
    } else {
        pace_percent(w, window_secs)
    }
}

/// Whether an account is currently usable (both windows have remaining quota).
pub fn is_available(u: &UsageInfo) -> bool {
    if u.account_limited || (u.primary.is_none() && u.secondary.is_none()) {
        return false;
    }
    if let Some(w) = &u.secondary
        && w.used_percent.unwrap_or(0.0) >= 100.0
    {
        return false;
    }
    if let Some(w) = &u.primary
        && w.used_percent.unwrap_or(0.0) >= 100.0
    {
        return false;
    }
    true
}

/// Eligibility check on a Candidate (reset-aware).
pub fn is_candidate_eligible(c: &Candidate, safety_margin_7d: f64) -> bool {
    if !c.has_5h_data && !c.has_7d_data {
        return false;
    }
    let used_5h = c.effective_used_5h();
    let used_7d = c.effective_used_7d();

    // Gate 1: 5h exhausted (and not past reset)
    if used_5h >= 100.0 {
        return false;
    }
    // Gate 2: 7d exhausted (and not past reset)
    if used_7d >= 100.0 {
        return false;
    }
    // Gate 3: 7d critically low and reset far away
    if c.has_7d_data {
        let remaining_7d = 100.0 - used_7d;
        let critical_pct = (safety_margin_7d * 0.25_f64).max(1.0);
        if remaining_7d < critical_pct {
            let hours_to_reset = c
                .resets_at_7d
                .map(|ts| ((ts - c.now) as f64 / 3600.0).max(0.0))
                .unwrap_or(f64::MAX);
            if hours_to_reset > 48.0 {
                return false;
            }
        }
    }
    // Gate 4: Free plan safety floor
    if c.is_free && c.has_5h_data {
        let remaining_5h = 100.0 - used_5h;
        if remaining_5h < FREE_FLOOR_PCT {
            return false;
        }
    }
    true
}

// ── adaptive scoring algorithm ─────────────────────────────

/// Adaptive scoring algorithm. Pure function, no I/O.
///
/// Automatically adjusts strategy based on pool state. No mode selection needed.
///
/// Components:
///   tier_bonus   — Team priority (0 or 500, configurable)
///   headroom     — Pace-aware effective remaining time (0..1100)
///   drain_value  — Quota that will be wasted if not used before reset (0..300)
///   sustain      — 7d budget-per-window sustainability (-800..0)
///   recency      — Spread usage across accounts (-60..0)
///
/// Pool-adaptive: drain_weight scales with pool_size and exhausted ratio.
pub fn score_unified(c: &Candidate, safety_margin_7d: f64) -> f64 {
    let used_5h = c.effective_used_5h();
    let used_7d = c.effective_used_7d();

    // ── Component A: tier_bonus (0 or 500) ──
    let tier_bonus = if c.is_team && c.team_priority {
        500.0
    } else {
        0.0
    };

    // ── Component B: headroom (0..1100) ──
    // Pace-aware: uses burn rate to project effective remaining time,
    // not just static remaining%.
    let headroom = if !c.has_5h_data {
        50.0
    } else if used_5h >= 100.0 {
        // Exhausted: score by time-to-reset (closer = higher, range 0..500).
        // The 500 ceiling (vs 1000+ for active accounts) is intentional:
        // is_candidate_eligible() marks exhausted accounts as ineligible,
        // and the caller sorts eligible-first. This branch only ranks among
        // ineligible fallback candidates when no eligible account exists.
        match c.resets_at_5h {
            None => 0.0,
            Some(reset_ts) => {
                let remaining_secs = (reset_ts - c.now).max(0) as f64;
                (500.0 - remaining_secs / 60.0).max(0.0)
            }
        }
    } else {
        // Pace-aware headroom: project remaining minutes using burn rate
        let remaining_pct = 100.0 - used_5h;
        match c.resets_at_5h {
            Some(reset_ts) => {
                let remaining_secs = (reset_ts - c.now).max(0) as f64;
                let elapsed_secs = (WINDOW_5H_SECS as f64 - remaining_secs).max(1.0);
                let burn_rate = used_5h / elapsed_secs; // %/sec

                if burn_rate > 0.001 {
                    // Project minutes until exhaustion at current rate
                    let projected_min = (remaining_pct / burn_rate) / 60.0;
                    // Cap at 300 min (5h), normalize to 0..100, add base 1000
                    1000.0 + (projected_min.min(300.0) / 300.0 * 100.0)
                } else {
                    // Near-zero burn rate → effectively full capacity
                    1000.0 + remaining_pct
                }
            }
            None => 1000.0 + remaining_pct,
        }
    };

    // ── Component C: sustain — 7d sustainability (-800..0) ──
    // Uses budget-per-window: how much 7d quota is available per remaining 5h window.
    const RELIEF_WINDOW_HOURS: f64 = 48.0;
    const MAX_RELIEF: f64 = 0.8;

    let sustain = if !c.has_7d_data {
        -50.0
    } else if used_7d >= 100.0 {
        // 7d exhausted: heavy penalty, relieved as reset approaches
        match c.resets_at_7d {
            None => -800.0, // no reset info: maximum penalty
            Some(reset_ts) => {
                let remaining_min = ((reset_ts - c.now).max(0) as f64) / 60.0;
                let relief = (1.0 - remaining_min / 10080.0).clamp(0.0, 1.0);
                -800.0 * (1.0 - relief)
            }
        }
    } else {
        let remaining_7d = 100.0 - used_7d;
        if remaining_7d >= safety_margin_7d {
            0.0
        } else {
            // Compute budget per remaining 5h window
            let budget_penalty = if let Some(reset_ts_7d) = c.resets_at_7d {
                let hours_to_7d_reset = ((reset_ts_7d - c.now) as f64 / 3600.0).max(0.0);
                let remaining_windows = (hours_to_7d_reset / 5.0).max(1.0);
                let budget_per_window = remaining_7d / remaining_windows;
                // If each window gets ≥ safety_margin worth of budget, it's fine
                if budget_per_window >= safety_margin_7d {
                    0.0
                } else {
                    // Shortfall: 0..1, higher = more pressure
                    ((safety_margin_7d - budget_per_window) / safety_margin_7d).clamp(0.0, 1.0)
                }
            } else {
                // No reset time: use simple pressure
                if safety_margin_7d > 0.0 {
                    ((safety_margin_7d - remaining_7d) / safety_margin_7d).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            };

            // Time relief: if 7d resets within 48h, reduce penalty
            let time_relief = c
                .resets_at_7d
                .map(|ts| {
                    let hours = ((ts - c.now) as f64 / 3600.0).max(0.0);
                    if hours < RELIEF_WINDOW_HOURS {
                        (1.0 - hours / RELIEF_WINDOW_HOURS).clamp(0.0, 1.0)
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);

            let effective = budget_penalty * (1.0 - time_relief * MAX_RELIEF);
            -800.0 * effective
        }
    };

    // ── Component D: drain_value (0..300) ──
    // Only activates when 5h reset is within 60 minutes AND there's quota to waste.
    // Pool-adaptive: larger pools with more available accounts → more aggressive drain.
    const DRAIN_WINDOW_MIN: f64 = 60.0;

    let raw_drain = if c.has_5h_data && used_5h < 100.0 {
        if let Some(reset_ts) = c.resets_at_5h {
            let remaining_min = ((reset_ts - c.now).max(0) as f64) / 60.0;
            if remaining_min <= DRAIN_WINDOW_MIN {
                let remaining_pct = 100.0 - used_5h;
                let urgency =
                    ((DRAIN_WINDOW_MIN - remaining_min) / DRAIN_WINDOW_MIN).clamp(0.0, 1.0);
                // waste = remaining quota × urgency, scaled to 0..300
                (remaining_pct * urgency * 3.0).min(300.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Pool-adaptive drain weight
    let drain_weight = if c.pool_size <= 2 {
        0.5 // Few accounts: be conservative, don't chase drain
    } else {
        let exhausted_ratio = c.pool_exhausted as f64 / c.pool_size as f64;
        if exhausted_ratio > 0.7 {
            0.3 // Most accounts exhausted: conserve what we have
        } else if c.pool_size >= 5 && exhausted_ratio < 0.3 {
            1.5 // Plenty of backup: drain aggressively
        } else {
            1.0
        }
    };

    let drain_value = raw_drain * drain_weight;

    // ── Component E: recency (-60..0) ──
    // Light spread penalty to avoid hammering the same account
    let recency = if c.last_used == 0 {
        0.0
    } else {
        let seconds_ago = (c.now - c.last_used).max(0) as f64;
        -(60.0 - (seconds_ago / 30.0)).clamp(0.0, 60.0)
    };

    tier_bonus + headroom + sustain + drain_value + recency
}

// ── Shared candidate building and selection ───────────────
//
// CLI `use` and automatic best selection score the same way through these
// helpers.

/// Build and score candidates uniformly: the API `plan_type` is
/// authoritative over the JWT (handles plan downgrades), and
/// `pool_exhausted` counts 5h-exhausted accounts across the whole input.
/// Input order is preserved.
pub fn score_candidates(
    fetched: Vec<(String, UsageInfo, crate::jwt::AccountInfo, i64)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<ScoredCandidate> {
    let pool_size = fetched.len();

    let mut candidates: Vec<(Candidate, UsageInfo)> = fetched
        .into_iter()
        .map(|(alias, u, info, last_used)| {
            let api_plan = u.plan_type.as_deref();
            let is_team = api_plan
                .map(|p| p == "team")
                .unwrap_or_else(|| info.is_team());
            let is_free = api_plan
                .map(|p| p == "free")
                .unwrap_or_else(|| info.is_free());
            let mut candidate = Candidate::from_usage(alias, &u, is_team, is_free, last_used, now);
            candidate.pool_size = pool_size;
            candidate.team_priority = team_priority;
            (candidate, u)
        })
        .collect();

    let pool_exhausted = candidates
        .iter()
        .filter(|(candidate, _)| candidate.effective_used_5h() >= 100.0)
        .count();
    for (candidate, _) in &mut candidates {
        candidate.pool_exhausted = pool_exhausted;
    }

    candidates
        .into_iter()
        .map(|(candidate, usage)| {
            let score = score_unified(&candidate, safety_7d);
            ScoredCandidate {
                candidate,
                usage,
                score,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_with(primary: Option<WindowUsage>, secondary: Option<WindowUsage>) -> UsageInfo {
        UsageInfo {
            fetched_at: None,
            primary,
            secondary,
            credits_balance: None,
            unlimited_credits: None,
            plan_type: None,
            reset_credits_available_count: None,
            reset_credits: vec![],
            reset_credits_error: None,
            account_limited: false,
            rate_limit_reached_type: None,
            individual_limit: None,
            additional_limits: vec![],
        }
    }

    fn window(used_percent: f64, resets_at: Option<i64>) -> WindowUsage {
        WindowUsage {
            used_percent: Some(used_percent),
            resets_at,
            window_minutes: None,
        }
    }

    #[test]
    fn test_default_usage_is_not_available() {
        assert!(!is_available(&UsageInfo::default()));
        let candidate = Candidate::from_usage(
            "empty".to_string(),
            &UsageInfo::default(),
            false,
            false,
            0,
            1,
        );
        assert!(!is_candidate_eligible(&candidate, 20.0));
    }

    #[test]
    fn test_is_available_both_under_100() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(is_available(&usage));
    }

    #[test]
    fn test_is_available_primary_exhausted() {
        let usage = usage_with(
            Some(window(100.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(!is_available(&usage));
    }

    #[test]
    fn test_is_available_secondary_exhausted() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(100.0, Some(2_000))),
        );

        assert!(!is_available(&usage));
    }

    #[test]
    fn test_is_available_no_data() {
        assert!(!is_available(&UsageInfo::default()));
    }

    // ── adaptive scoring tests ──

    fn make_candidate(
        alias: &str,
        used_5h: f64,
        reset_5h: Option<i64>,
        used_7d: f64,
        reset_7d: Option<i64>,
    ) -> Candidate {
        Candidate {
            alias: alias.to_string(),
            used_5h,
            resets_at_5h: reset_5h,
            used_7d,
            resets_at_7d: reset_7d,
            has_5h_data: true,
            has_7d_data: true,
            is_team: false,
            is_free: false,
            last_used: 0,
            now: 1_000_000,
            pool_size: 5,
            pool_exhausted: 0,
            team_priority: true,
        }
    }

    fn usage_with_5h(used_percent: f64, resets_at: i64, plan_type: Option<&str>) -> UsageInfo {
        UsageInfo {
            primary: Some(WindowUsage {
                used_percent: Some(used_percent),
                resets_at: Some(resets_at),
                window_minutes: None,
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(resets_at + 5 * 86400),
                window_minutes: None,
            }),
            plan_type: plan_type.map(|p| p.to_string()),
            ..UsageInfo::default()
        }
    }

    #[test]
    fn test_score_candidates_api_plan_overrides_jwt_and_counts_pool_exhausted() {
        let now = 1_000_000i64;
        let jwt_team = crate::jwt::AccountInfo {
            plan_type: Some("team".to_string()),
            ..Default::default()
        };
        let items = vec![
            // API says free although the JWT still claims team (plan downgrade)
            (
                "downgraded".to_string(),
                usage_with_5h(100.0, now + 3600, Some("free")),
                jwt_team,
                0,
            ),
            // No API plan — JWT (default: not team/free) applies
            (
                "healthy".to_string(),
                usage_with_5h(20.0, now + 3600, None),
                Default::default(),
                0,
            ),
        ];

        let scored = score_candidates(items, now, 20.0, true);

        assert_eq!(scored.len(), 2);
        assert_eq!(scored[0].candidate.alias, "downgraded"); // input order preserved
        assert!(scored[0].candidate.is_free);
        assert!(!scored[0].candidate.is_team);
        // One exhausted account (100% 5h), visible to every candidate
        assert_eq!(scored[0].candidate.pool_exhausted, 1);
        assert_eq!(scored[1].candidate.pool_exhausted, 1);
        assert_eq!(scored[1].candidate.pool_size, 2);
    }

    #[test]
    fn test_adaptive_prefers_more_remaining() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 60.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(score_unified(&a, 20.0) > score_unified(&b, 20.0));
    }

    #[test]
    fn test_adaptive_team_priority_dominates() {
        let now = 1_000_000i64;
        // Non-team with 0% used vs Team with 50% used → Team wins with priority
        let a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.is_team = true;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sb > sa,
            "team account should beat non-team even with worse 5h: {sb} > {sa}"
        );
    }

    #[test]
    fn test_adaptive_team_priority_disabled() {
        let now = 1_000_000i64;
        // With team_priority=false, Team should not get +500 bonus
        let mut a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        a.team_priority = false;
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.is_team = true;
        b.team_priority = false;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > sb,
            "without team_priority, more remaining should win: {sa} > {sb}"
        );
    }

    #[test]
    fn test_adaptive_drain_near_reset() {
        let now = 1_000_000i64;
        // Account A: 40% used, resets in 30 min (within drain window)
        let a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        // Account B: 40% used, resets in 4h (outside drain window)
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > sb,
            "near-reset account should score higher due to drain: {sa} > {sb}"
        );
    }

    #[test]
    fn test_adaptive_no_drain_outside_window() {
        let now = 1_000_000i64;
        // Both accounts reset in 2h+ (outside 60-min drain window)
        // A: 40% used, resets in 2h → elapsed 3h → burn=40/3h → low rate, more headroom
        // B: 40% used, resets in 4h → elapsed 1h → burn=40/1h → high rate, less headroom
        let a = make_candidate("a", 40.0, Some(now + 7200), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sa > 1000.0 && sb > 1000.0,
            "both should be usable: {sa}, {sb}"
        );
        // A consumed 40% over 3h (lower burn rate) → more projected headroom
        assert!(sa > sb, "lower burn rate gives more headroom: {sa} > {sb}");
    }

    #[test]
    fn test_adaptive_7d_critical_overrides_5h() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 0.0, Some(now + 18000), 95.0, Some(now + 6 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 7200), 30.0, Some(now + 5 * 86400));
        assert!(
            score_unified(&b, 20.0) > score_unified(&a, 20.0),
            "7d-critical should lose"
        );
    }

    #[test]
    fn test_adaptive_7d_budget_per_window() {
        let now = 1_000_000i64;
        // Account A: 7d 15% remaining, resets in 3 windows (15h) → 5%/window (tight)
        let a = make_candidate("a", 30.0, Some(now + 3600), 85.0, Some(now + 15 * 3600));
        // Account B: 7d 15% remaining, resets in 1 window (5h) → 15%/window (ok)
        let b = make_candidate("b", 30.0, Some(now + 3600), 85.0, Some(now + 5 * 3600));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(
            sb > sa,
            "higher budget-per-window should score better: {sb} > {sa}"
        );
    }

    #[test]
    fn test_adaptive_recency_breaks_tie() {
        let now = 1_000_000i64;
        let mut a = make_candidate("a", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        a.last_used = now - 5; // used 5 seconds ago
        let mut b = make_candidate("b", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        b.last_used = now - 1200; // used 20 minutes ago
        assert!(
            score_unified(&b, 20.0) > score_unified(&a, 20.0),
            "recently-used should score lower"
        );
    }

    #[test]
    fn test_adaptive_reset_aware() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 80.0, Some(now - 600), 20.0, Some(now + 5 * 86400));
        let score = score_unified(&a, 20.0);
        assert!(
            score > 1000.0,
            "past-reset account should score as fully available, got {score}"
        );
    }

    #[test]
    fn test_adaptive_exhausted_scores_low() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "exhausted should score much lower: {sb} > {sa}");
        assert!(sa < 500.0, "exhausted score should be low: {sa}");
    }

    #[test]
    fn test_adaptive_pool_exhausted_conservative_drain() {
        let now = 1_000_000i64;
        // Most accounts exhausted → drain weight should be low
        let mut a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        a.pool_size = 10;
        a.pool_exhausted = 8; // 80% exhausted
        let mut b = make_candidate("b", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        b.pool_size = 10;
        b.pool_exhausted = 1; // 10% exhausted
        // Both should have drain but b's pool allows more aggressive drain
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "healthy pool should allow more drain: {sb} > {sa}");
    }

    #[test]
    fn test_adaptive_free_floor_ineligible() {
        let now = 1_000_000i64;
        let mut c = make_candidate("free1", 70.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        c.is_free = true;
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_adaptive_no_data_low_score() {
        let c = Candidate {
            alias: "unknown".to_string(),
            used_5h: 0.0,
            resets_at_5h: None,
            used_7d: 0.0,
            resets_at_7d: None,
            has_5h_data: false,
            has_7d_data: false,
            is_team: false,
            is_free: false,
            last_used: 0,
            now: 1_000_000,
            pool_size: 1,
            pool_exhausted: 0,
            team_priority: true,
        };
        // headroom=50 (no 5h data) + sustain=-50 (no 7d data) = 0
        assert_eq!(
            score_unified(&c, 20.0),
            0.0,
            "no-data account should score exactly 0"
        );
    }

    #[test]
    fn test_adaptive_both_windows_exhausted() {
        let now = 1_000_000i64;
        // 5h exhausted (no reset info) + 7d exhausted (resets in 7 days)
        let mut c = make_candidate("both_dead", 100.0, None, 100.0, Some(now + 7 * 86400));
        c.has_5h_data = true;
        c.has_7d_data = true;
        let s = score_unified(&c, 20.0);
        // headroom=0 (exhausted, no reset), sustain should still be heavily negative
        assert!(
            s < -700.0,
            "doubly-exhausted account must score very low, got {s}"
        );
    }

    #[test]
    fn test_adaptive_both_windows_exhausted_no_reset_info() {
        // Worst case: both exhausted, no reset info at all
        let c = Candidate {
            alias: "dead".to_string(),
            used_5h: 100.0,
            resets_at_5h: None,
            used_7d: 100.0,
            resets_at_7d: None,
            has_5h_data: true,
            has_7d_data: true,
            is_team: false,
            is_free: false,
            last_used: 0,
            now: 1_000_000,
            pool_size: 1,
            pool_exhausted: 1,
            team_priority: false,
        };
        let s = score_unified(&c, 20.0);
        assert!(
            s < -700.0,
            "doubly-exhausted no-reset account must score very low, got {s}"
        );
    }

    #[test]
    fn test_adaptive_pace_aware_headroom() {
        let now = 1_000_000i64;
        // Account A: 30% used, resets in 4h → elapsed 1h → burn=30%/3600s (fast)
        // projected exhaustion = 70 / (30/3600) / 60 ≈ 140 min
        let a = make_candidate("a", 30.0, Some(now + 4 * 3600), 20.0, Some(now + 5 * 86400));
        // Account B: 30% used, resets in 1h → elapsed 4h → burn=30%/14400s (slow)
        // projected exhaustion = 70 / (30/14400) / 60 ≈ 560 min → capped 300 min
        let b = make_candidate("b", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        // B has slower burn rate → higher projected exhaustion → higher headroom
        assert!(
            sb > sa,
            "slower burn rate should give higher headroom: {sb} > {sa}"
        );
    }

    #[test]
    fn test_candidate_eligible_basic() {
        let now = 1_000_000i64;
        let c = make_candidate("ok", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_5h_exhausted() {
        let now = 1_000_000i64;
        let c = make_candidate("ex", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_7d_critical_far() {
        let now = 1_000_000i64;
        // 7d at 97% (3% remaining < critical 5%), resets in 5 days
        let c = make_candidate("crit", 30.0, Some(now + 3600), 97.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_eligible_7d_critical_near_reset() {
        let now = 1_000_000i64;
        // 7d at 97%, but resets in 12h → still eligible
        let c = make_candidate("near", 30.0, Some(now + 3600), 97.0, Some(now + 12 * 3600));
        assert!(is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_visible_pace_percent_hidden_when_ui_shows_zero_left() {
        let w = WindowUsage {
            used_percent: Some(99.6),
            resets_at: Some(auth::now_unix_secs() + 3600),
            window_minutes: None,
        };
        assert_eq!(visible_pace_percent(&w, WINDOW_5H_SECS), None);
    }

    #[test]
    fn test_visible_pace_percent_shown_when_remaining_exists() {
        let w = WindowUsage {
            used_percent: Some(99.4),
            resets_at: Some(auth::now_unix_secs() + 3600),
            window_minutes: None,
        };
        assert!(visible_pace_percent(&w, WINDOW_5H_SECS).is_some());
    }

    #[test]
    fn test_warmup_window_active_requires_elapsed_threshold() {
        let now = 1_000_000i64;
        let just_started = WindowUsage {
            used_percent: Some(1.0),
            resets_at: Some(now + WINDOW_5H_SECS - 60),
            window_minutes: None,
        };
        let past_threshold = WindowUsage {
            used_percent: Some(1.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };

        assert!(!warmup_window_active(&just_started, WINDOW_5H_SECS, now));
        assert!(warmup_window_active(&past_threshold, WINDOW_5H_SECS, now));
    }

    #[test]
    fn test_warmup_window_active_requires_real_usage() {
        let now = 1_000_000i64;
        let no_usage = WindowUsage {
            used_percent: Some(0.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let no_reset = WindowUsage {
            used_percent: Some(1.0),
            resets_at: None,
            window_minutes: None,
        };

        assert!(!warmup_window_active(&no_usage, WINDOW_5H_SECS, now));
        assert!(!warmup_window_active(&no_reset, WINDOW_5H_SECS, now));
    }

    #[test]
    fn test_paid_account_with_expired_5h_but_active_7d_is_not_already_warmed() {
        // Regression: previously OR-ed primary and secondary, so a paid account
        // whose 7d window was still active (the normal case after any real use)
        // would never re-warm after its 5h window expired.
        let now = 1_000_000i64;
        let expired_5h = WindowUsage {
            used_percent: Some(99.0),
            resets_at: Some(now - 60), // already reset server-side
            window_minutes: None,
        };
        let active_7d = WindowUsage {
            used_percent: Some(40.0),
            resets_at: Some(now + WINDOW_7D_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: Some(expired_5h),
            secondary: Some(active_7d),
            ..Default::default()
        };
        assert!(!usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_paid_account_with_active_5h_is_already_warmed() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            secondary: None,
            ..Default::default()
        };
        assert!(usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_inactive_future_model_pool_requires_warmup_even_when_main_pool_is_active() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let inactive_future_pool = super::super::AdditionalRateLimit {
            limit_name: Some("GPT-6-Codex-Burst".to_string()),
            metered_feature: Some("codex_futureburst".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: None,
            secondary: None,
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            additional_limits: vec![inactive_future_pool],
            ..Default::default()
        };

        assert!(!usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_code_review_pool_does_not_trigger_model_warmup() {
        let now = 1_000_000i64;
        let active_5h = WindowUsage {
            used_percent: Some(20.0),
            resets_at: Some(now + WINDOW_5H_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: Some(300),
        };
        let u = UsageInfo {
            primary: Some(active_5h),
            additional_limits: vec![super::super::AdditionalRateLimit {
                limit_name: Some("Code review".to_string()),
                metered_feature: Some("code_review".to_string()),
                allowed: Some(true),
                limit_reached: Some(false),
                primary: None,
                secondary: None,
            }],
            ..Default::default()
        };

        assert!(usage_has_active_warmup_window(&u, now));
    }

    #[test]
    fn test_free_account_falls_back_to_7d_window() {
        // Free accounts have primary=None (remapped to secondary in parse_usage).
        let now = 1_000_000i64;
        let active_7d = WindowUsage {
            used_percent: Some(10.0),
            resets_at: Some(now + WINDOW_7D_SECS - MIN_WARMUP_ELAPSED_SECS),
            window_minutes: None,
        };
        let u = UsageInfo {
            primary: None,
            secondary: Some(active_7d),
            ..Default::default()
        };
        assert!(usage_has_active_warmup_window(&u, now));
    }
}
