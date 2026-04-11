mod mock;

use codex_switch::usage::{self, Candidate};
use mock::scenarios;

/// Build a Candidate from a mock usage JSON response.
/// Parses the JSON through the real `parse_usage` path, then constructs a Candidate.
fn candidate_from_json(
    alias: &str,
    json: &serde_json::Value,
    is_team: bool,
    now: i64,
) -> Candidate {
    let usage = usage::parse_usage(json);
    Candidate::from_usage(alias.to_string(), &usage, is_team, false, 0, now)
}

/// Score and sort candidates, returning aliases in best-to-worst order.
fn rank(mut candidates: Vec<Candidate>, safety_margin_7d: f64) -> Vec<String> {
    let pool_size = candidates.len();
    let exhausted = candidates
        .iter()
        .filter(|c| c.effective_used_5h() >= 100.0)
        .count();
    for c in &mut candidates {
        c.pool_size = pool_size;
        c.pool_exhausted = exhausted;
    }
    let mut scored: Vec<(Candidate, f64)> = candidates
        .into_iter()
        .map(|c| {
            let s = usage::score_unified(&c, safety_margin_7d);
            (c, s)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored.into_iter().map(|(c, _)| c.alias).collect()
}

// ── Test: healthy_pool ordering ──
// 3 Plus accounts at 0%, 30%, 60%. The least-used account should rank first.
#[test]
fn healthy_pool_prefers_least_used() {
    let scenario = scenarios::healthy_pool();
    let now = codex_switch::auth::now_unix_secs();

    let candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            candidate_from_json(alias, &responses[0], false, now)
        })
        .collect();

    let ranking = rank(candidates, 20.0);

    // 0% used should be first, 60% used should be last
    assert_eq!(ranking[0], "healthy_a", "least used (0%) should rank first");
    assert_eq!(ranking[2], "healthy_c", "most used (60%) should rank last");
}

// ── Test: team_priority ──
// Team(50%) should outrank Plus(10%) when team_priority is enabled.
#[test]
fn team_priority_outranks_plus() {
    let scenario = scenarios::team_priority();
    let now = codex_switch::auth::now_unix_secs();

    let mut candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            let is_team = alias == "team";
            candidate_from_json(alias, &responses[0], is_team, now)
        })
        .collect();

    // Enable team_priority
    for c in &mut candidates {
        c.team_priority = true;
    }

    let ranking = rank(candidates, 20.0);

    assert_eq!(
        ranking[0], "team",
        "team account should rank first with team_priority enabled"
    );
}

// ── Test: drain_window ──
// A(40%, resets 20min) should rank higher than B(30%, resets 4h) due to drain value.
// A has quota that will be wasted if not used before its imminent reset.
#[test]
fn drain_window_prefers_soon_to_reset() {
    let scenario = scenarios::drain_window();
    let now = codex_switch::auth::now_unix_secs();

    let candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            candidate_from_json(alias, &responses[0], false, now)
        })
        .collect();

    let ranking = rank(candidates, 20.0);

    assert_eq!(
        ranking[0], "drain_a",
        "account resetting in 20min should be drained first"
    );
}

// ── Test: seven_day_crisis ──
// A(5h:0%, 7d:95%) vs B(5h:50%, 7d:30%).
// Despite A having 0% 5h usage, the 7d crisis should penalize it.
#[test]
fn seven_day_crisis_penalizes_near_7d_limit() {
    let scenario = scenarios::seven_day_crisis();
    let now = codex_switch::auth::now_unix_secs();

    let candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            candidate_from_json(alias, &responses[0], false, now)
        })
        .collect();

    let ranking = rank(candidates, 20.0);

    assert_eq!(
        ranking[0], "7d_crisis_b",
        "account B (low 7d usage) should rank above A (95% 7d usage)"
    );
}

// ── Test: gradual_exhaustion timeline ──
// A starts at 30% and exhausts over 4 ticks. B stays at 20%.
// Verify that as A exhausts, B eventually ranks higher.
#[test]
fn gradual_exhaustion_shifts_preference() {
    let scenario = scenarios::gradual_exhaustion();
    let now = codex_switch::auth::now_unix_secs();

    // Tick 0: A=30%, B=20% — both are healthy, A has more headroom in 5h window
    // because both have similar reset times but A starts with a higher burn rate.
    // The relative ranking depends on the burn rate calculation.
    let a_responses = &scenario[0].1;
    let b_responses = &scenario[1].1;

    // Tick 3: A=100%, B=20% — A is exhausted, B should rank first
    let candidates = vec![
        candidate_from_json("gradual_a", &a_responses[3], false, now),
        candidate_from_json("gradual_b", &b_responses[3], false, now),
    ];

    let ranking = rank(candidates, 20.0);
    assert_eq!(
        ranking[0], "gradual_b",
        "B should rank first when A is exhausted at tick 3"
    );

    // Tick 2: A=90%, B=20% — A is near exhaustion, B should rank first
    let candidates = vec![
        candidate_from_json("gradual_a", &a_responses[2], false, now),
        candidate_from_json("gradual_b", &b_responses[2], false, now),
    ];

    let ranking = rank(candidates, 20.0);
    assert_eq!(
        ranking[0], "gradual_b",
        "B should rank first when A is at 90% at tick 2"
    );

    // Tick 0: A=30%, B=20% — both healthy. A has slightly higher burn rate
    // (30%/3600s vs 20%/1800s elapsed) but both have ample headroom.
    // The exact ranking at tick 0 depends on pace-aware projections.
    // Key invariant: both should be eligible (scored > 0).
    let candidates_t0 = vec![
        candidate_from_json("gradual_a", &a_responses[0], false, now),
        candidate_from_json("gradual_b", &b_responses[0], false, now),
    ];
    let s_a = usage::score_unified(&candidates_t0[0], 20.0);
    let s_b = usage::score_unified(&candidates_t0[1], 20.0);
    assert!(s_a > 0.0, "A should be eligible at tick 0 (score={s_a})");
    assert!(s_b > 0.0, "B should be eligible at tick 0 (score={s_b})");
}

// ── Test: all_exhausted selects soonest reset ──
// When all accounts are at 100%, prefer the one resetting soonest.
#[test]
fn all_exhausted_prefers_soonest_reset() {
    let scenario = scenarios::all_exhausted();
    let now = codex_switch::auth::now_unix_secs();

    let candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            candidate_from_json(alias, &responses[0], false, now)
        })
        .collect();

    let ranking = rank(candidates, 20.0);

    assert_eq!(
        ranking[0], "exhausted_a",
        "account resetting in 30min should rank first among all exhausted"
    );
}

// ── Test: team_exhausted falls back to plus ──
// When the team account is at 100%, plus accounts should rank above it.
#[test]
fn team_exhausted_falls_back_to_plus() {
    let scenario = scenarios::team_exhausted();
    let now = codex_switch::auth::now_unix_secs();

    let mut candidates: Vec<Candidate> = scenario
        .iter()
        .map(|(token, responses)| {
            let alias = token.strip_prefix("tok_").unwrap_or(token);
            let is_team = alias == "team_exhausted";
            candidate_from_json(alias, &responses[0], is_team, now)
        })
        .collect();

    for c in &mut candidates {
        c.team_priority = true;
    }

    let ranking = rank(candidates, 20.0);

    // Team is exhausted (100%), so plus accounts should rank above it
    assert_ne!(
        ranking[0], "team_exhausted",
        "exhausted team account should not rank first"
    );
}
