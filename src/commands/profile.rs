use super::render::{confirm_default_no, print_usage_line};
use crate::output::{
    self, ProgressReporter, account_to_json, print_json, usage_to_json, user_println,
};
use crate::{auth, cache, color, config, jwt, profile, usage, workspace};
use anyhow::{Context, Result};

/// Surface profiles whose rotated credentials could not be written.
///
/// The auth server has already invalidated their previous refresh token, so
/// staying quiet hands the user an account that stops working later with no
/// clue why. Printed to stderr so `--json` stdout stays machine-readable.
fn report_token_persist_failures(failures: &[usage::TokenPersistFailure]) {
    for failure in failures {
        eprintln!(
            "{}",
            color::error(&format!("Warning: {}", failure.error.detail))
        );
    }
}

// ── use ──────────────────────────────────────────────────

pub(crate) async fn use_cmd(alias: Option<&str>, json: bool, consume_card: bool) -> Result<()> {
    use std::io::IsTerminal;

    match alias {
        Some(a) => {
            profile::cmd_use(a, !json && std::io::stdin().is_terminal())?;
            cache::set_last_used(a)?;
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a.to_string(),
                    action: "switched".into(),
                });
            }
        }
        None => best_cmd(json, consume_card).await?,
    }
    Ok(())
}

// ── list (all profiles + usage, concurrent) ──────────────

pub(crate) async fn list_cmd(force: bool, json: bool, auth_already_handled: bool) -> Result<()> {
    if !auth_already_handled {
        profile::auto_track_current();
    }

    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json {
            print_json(&output::JsonUsageResult { profiles: vec![] });
        } else {
            println!("{}", color::dim("(no saved profiles)"));
        }
        return Ok(());
    }

    let current = profile::read_current();

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    struct ListRow {
        name: String,
        is_current: bool,
        info: jwt::AccountInfo,
        usage_result: Option<std::result::Result<usage::UsageInfo, usage::UsageError>>,
    }

    let mut rows: Vec<ListRow> = profiles
        .into_iter()
        .filter_map(|name| {
            let path = match profile::profile_auth_path(&name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{name}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            let info = auth::read_account_info(&path);
            let usage_result = if force {
                None
            } else {
                cache::get(&name).map(Ok)
            };
            Some(ListRow {
                is_current: name == current,
                name,
                info,
                usage_result,
            })
        })
        .collect();

    let refresh_count = rows.iter().filter(|row| row.usage_result.is_none()).count();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Refreshing usage", refresh_count))
    };

    let mut tasks = tokio::task::JoinSet::new();
    for (idx, row) in rows.iter().enumerate() {
        let needs_usage = row.usage_result.is_none();
        let needs_workspace = force
            || row
                .info
                .account_id
                .as_deref()
                .is_some_and(|id| !cache::workspace_name_is_known(id));
        if !needs_usage && !needs_workspace {
            continue;
        }

        let alias = row.name.clone();
        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (
                    idx,
                    needs_usage.then(|| {
                        Err(usage::UsageError {
                            summary: "limiter closed".into(),
                            detail: "usage limiter closed".into(),
                        })
                    }),
                );
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        idx,
                        needs_usage.then(|| {
                            Err(usage::UsageError {
                                summary: format!("path error: {e}"),
                                detail: format!("failed to resolve profile path: {e}"),
                            })
                        }),
                    );
                }
            };
            let usage_result = if needs_usage {
                Some(if force {
                    usage::fetch_usage_retried_force(&alias, &path, &current).await
                } else {
                    usage::fetch_usage_retried(&alias, &path, &current).await
                })
            } else {
                None
            };
            // Read auth after usage: that path may have refreshed and persisted the token.
            if let Ok(auth) = auth::read_auth(&path)
                && let Err(err) = workspace::refresh_for_auth_if_needed(&auth, force).await
            {
                tracing::debug!("[{alias}] workspace metadata unavailable: {err}");
            }
            (idx, usage_result)
        });
    }

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        let (idx, usage_result) = task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?;
        if let Some(usage_result) = usage_result {
            rows[idx].usage_result = Some(usage_result);
            completed += 1;
        }
        cache::apply_workspace_name(&mut rows[idx].info);
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut json_items = vec![];

    for row in rows {
        let usage_result = row.usage_result.unwrap_or_else(|| {
            Err(usage::UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            })
        });
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(&e.detail)),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: row.name,
                is_current: row.is_current,
                account: account_to_json(
                    &row.info,
                    usage_result
                        .as_ref()
                        .ok()
                        .and_then(|u| u.plan_type.as_deref()),
                ),
                usage: ju,
            });
        } else {
            let mark = if row.is_current {
                color::active("*")
            } else {
                " ".to_string()
            };
            let alias_str = if row.is_current {
                color::bold(&row.name)
            } else {
                row.name.clone()
            };
            print!("{mark} {alias_str}");
            if let Some(email) = &row.info.email {
                print!("  {}", color::dim(email));
            }
            // API plan_type is authoritative over JWT claims (handles plan downgrades)
            let effective_plan = if let Ok(u) = &usage_result {
                u.plan_type.as_deref().or(row.info.plan_type.as_deref())
            } else {
                row.info.plan_type.as_deref()
            };
            if effective_plan.is_some() {
                let label = if let Ok(u) = &usage_result
                    && u.plan_type.is_some()
                {
                    row.info.plan_label_with(u.plan_type.as_deref())
                } else {
                    row.info.plan_label()
                };
                print!("  {}", color::plan(&label, effective_plan));
            }
            println!();
            match usage_result {
                Ok(u) => print_usage_line(&u),
                Err(e) => println!("  {} {}", color::error("!!"), color::error(&e.summary)),
            }
            println!(); // blank line between accounts
        }
    }

    if json {
        print_json(&output::JsonUsageResult {
            profiles: json_items,
        });
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

    Ok(())
}

// ── rename ───────────────────────────────────────────────

pub(crate) fn rename_cmd(old: &str, new: &str, json: bool) -> Result<()> {
    profile::rename_profile(old, new)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: new.to_string(),
            action: "renamed".into(),
        });
    }
    Ok(())
}

pub(crate) fn delete_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    profile::validate_alias(alias)?;
    if profile::read_current() == alias {
        anyhow::bail!("cannot delete the active profile '{alias}'");
    }
    if !profile::profile_auth_path(alias)?.exists() {
        anyhow::bail!("profile '{alias}' not found");
    }

    if !yes {
        if json || !std::io::stdin().is_terminal() {
            anyhow::bail!("confirmation required; rerun with --yes to delete profile '{alias}'");
        }
        if !confirm_default_no(&format!(
            "Delete profile '{alias}'? It will remain recoverable. [y/N] "
        )) {
            user_println("Deletion cancelled.");
            return Ok(());
        }
    }
    profile::cmd_delete(alias)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "deleted".into(),
        });
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<(usage::Candidate, usage::UsageInfo, f64)> {
    let items = fetched
        .into_iter()
        .map(|(alias, u)| {
            let info = profile::profile_auth_path(&alias)
                .map(|p| auth::read_account_info(&p))
                .unwrap_or_default();
            let last_used = cache::get_last_used(&alias);
            (alias, u, info, last_used)
        })
        .collect();

    let mut scored: Vec<(usage::Candidate, usage::UsageInfo, f64)> =
        usage::score_candidates(items, now, safety_7d, team_priority)
            .into_iter()
            .map(|s| (s.candidate, s.usage, s.score))
            .collect();

    scored.sort_by(|a, b| {
        let eligible_a = usage::is_candidate_eligible(&a.0, safety_7d);
        let eligible_b = usage::is_candidate_eligible(&b.0, safety_7d);
        eligible_b
            .cmp(&eligible_a)
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.last_used.cmp(&b.0.last_used))
            .then(a.0.alias.cmp(&b.0.alias))
    });

    scored
}

// ── reset-card-aware revival ──────────────────────────────

/// How aggressively the pool-exhausted fallback may consume a reset card to
/// revive an otherwise-ineligible account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardPolicy {
    /// Ask the user interactively before consuming a card.
    Prompt,
    /// Consume without asking (user passed --consume-card, or already confirmed).
    PreApproved,
    /// Never consume; surface a hint instead (JSON / non-TTY without the flag).
    Deny,
}

/// Surfaced to the caller when the pool was exhausted, an account held a
/// reset card, but nothing was consumed (denied or declined).
pub(crate) struct RevivalHint {
    pub(crate) alias: String,
    pub(crate) card_count: u64,
    pub(crate) consumed_unconfirmed: Option<&'static str>,
    pub(crate) consumption_unknown_message: Option<String>,
}

pub(crate) struct SelectOutcome {
    pub(crate) alias: String,
    pub(crate) usage: usage::UsageInfo,
    pub(crate) score: f64,
    pub(crate) revival_hint: Option<RevivalHint>,
}

pub(crate) fn revival_hint_message(hint: &RevivalHint) -> String {
    if let Some(message) = &hint.consumption_unknown_message {
        return message.clone();
    }
    if let Some(summary) = hint.consumed_unconfirmed {
        return format!(
            "{}: card was consumed, but account could not be confirmed revived ({summary})",
            hint.alias
        );
    }
    format!(
        "{} holds {} reset card(s); rerun with --consume-card to revive",
        hint.alias, hint.card_count
    )
}

/// Interactive confirmation prompt text for reviving an account by consuming
/// its earliest-expiring reset card. Pure formatting, no I/O.
fn revival_prompt_message(alias: &str, card_count: u64, earliest_expiry: &str) -> String {
    format!(
        "'{alias}' holds {card_count} reset card(s) (earliest expiry {earliest_expiry}); consume one to revive it? [y/N] "
    )
}

/// One scored candidate as seen by `pick_revival_target`. Pure data, no I/O.
struct RevivalCandidate<'a> {
    alias: &'a str,
    eligible: bool,
    score: f64,
    reset_credits: &'a [usage::ResetCredit],
}

/// Sort key for a credit's expiry: missing expiry sorts as "latest" (never
/// ahead of a dated card), matching `usage::earliest_reset_credit`.
fn expiry_sort_key(credit: &usage::ResetCredit) -> i64 {
    credit
        .expires_at
        .as_deref()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(i64::MAX)
}

/// Pick which ineligible, card-holding account should be revived by
/// consuming its earliest-expiring reset card.
///
/// Meaningful only when none of `candidates` are eligible (caller-guaranteed).
/// Ties break by card count (more cards first), then by existing score.
fn pick_revival_target(candidates: &[RevivalCandidate]) -> Option<String> {
    candidates
        .iter()
        .filter(|c| !c.eligible && !c.reset_credits.is_empty())
        .filter_map(|c| {
            let earliest = usage::earliest_reset_credit(c.reset_credits)?;
            Some((c, expiry_sort_key(earliest)))
        })
        .min_by(|(a, a_key), (b, b_key)| {
            a_key
                .cmp(b_key)
                .then_with(|| b.reset_credits.len().cmp(&a.reset_credits.len()))
                .then_with(|| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        })
        .map(|(c, _)| c.alias.to_string())
}

pub(crate) async fn select_best_profile(
    json: bool,
    card_policy: CardPolicy,
) -> Result<SelectOutcome> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        anyhow::bail!(
            "no saved profiles; run `codex-switch login` or `codex-switch import <path>` first"
        );
    }

    let current = profile::read_current();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut tasks = tokio::task::JoinSet::new();
    let mut fetched: Vec<(String, usage::UsageInfo)> = Vec::with_capacity(profiles.len());

    for alias in profiles {
        if let Some(cached) = cache::get_async(&alias).await {
            fetched.push((alias, cached));
            continue;
        }

        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{alias}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            match usage::fetch_usage_retried(&alias, &path, &current).await {
                Ok(u) => Some((alias, u)),
                Err(e) => {
                    tracing::warn!("[{alias}] usage fetch failed during auto-select: {e}");
                    None
                }
            }
        });
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Testing accounts", tasks.len()))
    };

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        completed += 1;
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
        if let Some((alias, usage)) =
            task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?
        {
            fetched.push((alias, usage));
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    if fetched.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    let safety_7d = config::get().use_cfg.safety_margin_7d;
    let team_priority = config::get().use_cfg.team_priority;
    let now = auth::now_unix_secs();
    let scored = score_profile_candidates(fetched, now, safety_7d, team_priority);
    let (top_candidate, top_usage, top_score) = scored
        .first()
        .map(|(c, u, s)| (c.clone(), u.clone(), *s))
        .context("failed to select best profile")?;

    if usage::is_candidate_eligible(&top_candidate, safety_7d) {
        return Ok(SelectOutcome {
            alias: top_candidate.alias,
            usage: top_usage,
            score: top_score,
            revival_hint: None,
        });
    }

    // Pool exhausted: see if a card-holding account can be revived.
    let revival_candidates: Vec<RevivalCandidate> = scored
        .iter()
        .map(|(c, u, s)| RevivalCandidate {
            alias: &c.alias,
            eligible: usage::is_candidate_eligible(c, safety_7d),
            score: *s,
            reset_credits: &u.reset_credits,
        })
        .collect();
    let revival_target = pick_revival_target(&revival_candidates);

    let Some(target_alias) = revival_target else {
        return Ok(SelectOutcome {
            alias: top_candidate.alias,
            usage: top_usage,
            score: top_score,
            revival_hint: None,
        });
    };

    let target_candidate = scored
        .iter()
        .find(|(c, _, _)| c.alias == target_alias)
        .map(|(c, u, _)| (c.clone(), u.clone()))
        .context("revival target disappeared from scored candidates")?;
    let (target_candidate, target_usage) = target_candidate;
    let card_count = target_usage.reset_credits.len() as u64;
    let selected_credit = usage::earliest_reset_credit(&target_usage.reset_credits)
        .context("revival target no longer has an available reset card")?;

    let approved = match card_policy {
        CardPolicy::Deny => false,
        CardPolicy::PreApproved => true,
        CardPolicy::Prompt => {
            let expires = selected_credit
                .expires_at
                .as_deref()
                .map(output::format_local_datetime)
                .unwrap_or_else(|| "no expiry".to_string());
            confirm_default_no(&revival_prompt_message(&target_alias, card_count, &expires))
        }
    };

    let fallback = |hint: Option<RevivalHint>| SelectOutcome {
        alias: top_candidate.alias.clone(),
        usage: top_usage.clone(),
        score: top_score,
        revival_hint: hint,
    };

    if !approved {
        return Ok(fallback(Some(RevivalHint {
            alias: target_alias,
            card_count,
            consumed_unconfirmed: None,
            consumption_unknown_message: None,
        })));
    }

    let target_path = profile::profile_auth_path(&target_alias)?;
    let current = profile::read_current();
    match usage::consume_reset_credit_by_id(&target_alias, &target_path, &selected_credit.id).await
    {
        Ok(_consumed) => {
            if let Err(err) = cache::invalidate(&target_alias) {
                tracing::warn!("Failed to invalidate usage cache for {target_alias}: {err}");
            }
            let failure_summary = match usage::fetch_usage_retried_force(
                &target_alias,
                &target_path,
                &current,
            )
            .await
            {
                Ok(revived_usage) => {
                    let mut revived_candidate = usage::Candidate::from_usage(
                        target_alias.clone(),
                        &revived_usage,
                        target_candidate.is_team,
                        target_candidate.is_free,
                        target_candidate.last_used,
                        now,
                    );
                    revived_candidate.pool_size = target_candidate.pool_size;
                    revived_candidate.team_priority = target_candidate.team_priority;
                    revived_candidate.pool_exhausted =
                        target_candidate.pool_exhausted.saturating_sub(1);
                    if usage::is_candidate_eligible(&revived_candidate, safety_7d) {
                        let score = usage::score_unified(&revived_candidate, safety_7d);
                        return Ok(SelectOutcome {
                            alias: target_alias,
                            usage: revived_usage,
                            score,
                            revival_hint: None,
                        });
                    }
                    tracing::warn!(
                        "[{target_alias}] still exhausted after consuming a reset card; not consuming a second card"
                    );
                    "quota remained exhausted after refresh"
                }
                Err(e) => {
                    tracing::warn!(
                        "[{target_alias}] failed to refresh usage after consuming reset card: {e}"
                    );
                    "usage refresh failed"
                }
            };
            return Ok(fallback(Some(RevivalHint {
                alias: target_alias,
                card_count,
                consumed_unconfirmed: Some(failure_summary),
                consumption_unknown_message: None,
            })));
        }
        Err(e) => {
            tracing::warn!("[{target_alias}] failed to consume reset card: {e}");
            if e.outcome_unknown_after_request() {
                if let Err(err) = cache::invalidate(&target_alias) {
                    tracing::warn!("Failed to invalidate usage cache for {target_alias}: {err}");
                }
                let message = e.user_facing_unknown_message(&target_alias);
                return Ok(fallback(Some(RevivalHint {
                    alias: target_alias,
                    card_count,
                    consumed_unconfirmed: None,
                    consumption_unknown_message: Some(message),
                })));
            } else {
                debug_assert!(e.definitely_not_consumed());
            }
        }
    }

    Ok(fallback(None))
}

async fn best_cmd(json: bool, consume_card: bool) -> Result<()> {
    use std::io::IsTerminal;

    let card_policy = if consume_card {
        CardPolicy::PreApproved
    } else if !json && std::io::stdin().is_terminal() {
        CardPolicy::Prompt
    } else {
        CardPolicy::Deny
    };

    let outcome = select_best_profile(json, card_policy).await?;
    let SelectOutcome {
        alias: best_alias,
        usage: best_usage,
        score: best_score,
        revival_hint,
    } = outcome;

    profile::switch_profile(&best_alias)?;
    cache::set_last_used(&best_alias)?;

    let path = profile::profile_auth_path(&best_alias)?;
    let info = auth::read_account_info(&path);

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&info, best_usage.plan_type.as_deref()),
            usage: usage_to_json(Ok(&best_usage)),
            score: best_score,
            mode: "unified".to_string(),
            hint: revival_hint.as_ref().map(revival_hint_message),
        });
    } else {
        println!("{}", color::success(&format!("Switched to: {best_alias}")));
        print_usage_line(&best_usage);
        if let Some(hint) = &revival_hint {
            println!("  {}", color::dim(&revival_hint_message(hint)));
        }
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    report_token_persist_failures(&usage::refresh_expiring_tokens().await);

    Ok(())
}

// ── tests: pick_revival_target ────────────────────────────

#[cfg(test)]
mod revival_target_tests {
    use super::*;

    fn credit(id: &str, expires_at: Option<&str>) -> usage::ResetCredit {
        usage::ResetCredit {
            id: id.to_string(),
            granted_at: None,
            expires_at: expires_at.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_pick_revival_target_returns_none_when_nobody_holds_card() {
        let no_cards: Vec<usage::ResetCredit> = vec![];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &no_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &no_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates), None);
    }

    #[test]
    fn test_pick_revival_target_returns_earliest_expiring_card_holder() {
        let a_cards = vec![credit("a1", Some("2026-07-10T00:00:00Z"))];
        let b_cards = vec![credit("b1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));
    }

    #[test]
    fn test_pick_revival_target_treats_missing_expiry_as_latest() {
        let a_cards = vec![credit("a1", None)]; // never expires -> sorts as latest
        let b_cards = vec![credit("b1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 10.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 20.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));
    }

    #[test]
    fn test_pick_revival_target_tie_breaks_by_card_count_then_score() {
        // Same earliest expiry: a has 1 card, b has 2 cards -> b wins (more cards).
        let a_cards = vec![credit("a1", Some("2026-07-05T00:00:00Z"))];
        let b_cards = vec![
            credit("b1", Some("2026-07-05T00:00:00Z")),
            credit("b2", Some("2026-07-20T00:00:00Z")),
        ];
        let candidates = vec![
            RevivalCandidate {
                alias: "a",
                eligible: false,
                score: 50.0,
                reset_credits: &a_cards,
            },
            RevivalCandidate {
                alias: "b",
                eligible: false,
                score: 10.0,
                reset_credits: &b_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates).as_deref(), Some("b"));

        // Same earliest expiry, same card count -> higher score wins.
        let c_cards = vec![credit("c1", Some("2026-07-05T00:00:00Z"))];
        let d_cards = vec![credit("d1", Some("2026-07-05T00:00:00Z"))];
        let candidates2 = vec![
            RevivalCandidate {
                alias: "c",
                eligible: false,
                score: 5.0,
                reset_credits: &c_cards,
            },
            RevivalCandidate {
                alias: "d",
                eligible: false,
                score: 15.0,
                reset_credits: &d_cards,
            },
        ];
        assert_eq!(pick_revival_target(&candidates2).as_deref(), Some("d"));
    }

    #[test]
    fn test_revival_prompt_message_includes_alias_count_and_expiry() {
        let msg = revival_prompt_message("acct-a", 2, "07-08 00:00");
        assert!(msg.contains("acct-a"));
        assert!(msg.contains('2'));
        assert!(msg.contains("07-08 00:00"));
        assert!(msg.contains("[y/N]"));
    }

    #[test]
    fn test_revival_hint_message_includes_alias_and_flag() {
        let hint = RevivalHint {
            alias: "acct-b".to_string(),
            card_count: 3,
            consumed_unconfirmed: None,
            consumption_unknown_message: None,
        };
        let msg = revival_hint_message(&hint);
        assert!(msg.contains("acct-b"));
        assert!(msg.contains('3'));
        assert!(msg.contains("--consume-card"));
    }

    #[test]
    fn test_pick_revival_target_ignores_eligible_candidates() {
        let cards = vec![credit("x1", Some("2026-07-05T00:00:00Z"))];
        let candidates = vec![RevivalCandidate {
            alias: "eligible_holder",
            eligible: true,
            score: 999.0,
            reset_credits: &cards,
        }];
        assert_eq!(pick_revival_target(&candidates), None);
    }
}
