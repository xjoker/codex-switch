//! HTTP-level integration tests using the mock server.
//!
//! These tests start a real HTTP mock server, create temp profile directories
//! with fake auth.json files, and call the mock directly via reqwest to verify
//! the HTTP → parse → score pipeline.

mod mock;

use codex_switch::auth;
use codex_switch::usage::{self, Candidate};
use mock::scenarios;
use serde_json::json;
use std::path::PathBuf;

/// Create a temp directory with fake profile auth.json files.
/// Returns (temp_dir, vec of (alias, path, token, is_team)).
fn setup_profiles(
    entries: &[(String, Vec<serde_json::Value>)],
) -> (tempfile::TempDir, Vec<(String, PathBuf, String, bool)>) {
    let dir = tempfile::tempdir().unwrap();
    let mut profiles = Vec::new();

    for (token, responses) in entries {
        let alias = token.strip_prefix("tok_").unwrap_or(token).to_string();
        let profile_dir = dir.path().join(&alias);
        std::fs::create_dir_all(&profile_dir).unwrap();

        let auth_json = json!({
            "tokens": {
                "access_token": token,
                "refresh_token": format!("refresh_{token}"),
                "id_token": "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.fake"
            }
        });
        let auth_path = profile_dir.join("auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_json).unwrap(),
        )
        .unwrap();

        // Detect team from first response
        let is_team = responses
            .first()
            .and_then(|r| r.get("plan_type"))
            .and_then(|v| v.as_str())
            .map(|s| s == "team")
            .unwrap_or(false);

        profiles.push((alias, auth_path, token.clone(), is_team));
    }

    (dir, profiles)
}

/// Helper: fetch usage from mock, parse, build candidates, compute pool state, score, and rank.
/// Returns (alias, score) sorted best-first.
fn score_from_responses(
    responses: &[(String, serde_json::Value)],
    profiles: &[(String, PathBuf, String, bool)],
    team_priority: bool,
    safety_margin_7d: f64,
    now: i64,
) -> Vec<(String, f64)> {
    let pool_size = responses.len();

    let mut candidates: Vec<Candidate> = responses
        .iter()
        .map(|(alias, body)| {
            let is_team = profiles.iter().find(|(a, _, _, _)| a == alias).unwrap().3;
            let u = usage::parse_usage(body);
            let mut c = Candidate::from_usage(alias.clone(), &u, is_team, false, 0, now);
            c.pool_size = pool_size;
            c.team_priority = team_priority;
            c
        })
        .collect();

    // Compute pool_exhausted dynamically
    let pool_exhausted = candidates
        .iter()
        .filter(|c| c.effective_used_5h() >= 100.0)
        .count();
    for c in &mut candidates {
        c.pool_exhausted = pool_exhausted;
    }

    let mut scored: Vec<(String, f64)> = candidates
        .into_iter()
        .map(|c| {
            let s = usage::score_unified(&c, safety_margin_7d);
            (c.alias, s)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    scored
}

/// Fetch all profiles from mock server and return (alias, response_body) pairs.
async fn fetch_all(
    client: &reqwest::Client,
    url: &str,
    profiles: &[(String, PathBuf, String, bool)],
) -> Vec<(String, serde_json::Value)> {
    let mut results = Vec::new();
    for (alias, _path, token, _is_team) in profiles {
        let resp = client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "HTTP request for {alias} failed: {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        results.push((alias.clone(), body));
    }
    results
}

// ── Tests ──

#[tokio::test]
async fn http_healthy_pool_ranking() {
    let entries = scenarios::healthy_pool();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, true, 20.0, now);

    assert_eq!(scored[0].0, "healthy_a", "0% used should rank first");
    assert_eq!(scored[2].0, "healthy_c", "60% used should rank last");

    // Verify scores are in the usable tier
    for (alias, score) in &scored {
        assert!(
            *score > 1000.0,
            "{alias} should be in usable tier, got {score}"
        );
    }

    server.shutdown();
}

#[tokio::test]
async fn http_team_priority() {
    let entries = scenarios::team_priority();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, true, 20.0, now);

    assert_eq!(
        scored[0].0, "team",
        "team should rank first with +500 bonus"
    );
    // Team score should be 500+ higher than plus accounts
    assert!(
        scored[0].1 - scored[1].1 > 400.0,
        "team bonus should create large gap"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_drain_window() {
    let entries = scenarios::drain_window();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "drain_a",
        "20min-to-reset should be drained first"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_seven_day_crisis() {
    let entries = scenarios::seven_day_crisis();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "7d_crisis_b",
        "healthy 7d should outrank 95% 7d"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_all_exhausted() {
    let entries = scenarios::all_exhausted();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    let responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    // pool_exhausted is computed dynamically inside score_from_responses
    let scored = score_from_responses(&responses, &profiles, false, 20.0, now);

    assert_eq!(
        scored[0].0, "exhausted_a",
        "soonest reset (30min) should rank first"
    );
    assert!(
        scored[0].1 < 500.0,
        "exhausted accounts should be in low tier"
    );

    server.shutdown();
}

#[tokio::test]
async fn http_timeline_gradual_exhaustion() {
    let entries = scenarios::gradual_exhaustion();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let now = auth::now_unix_secs();

    // Tick 0: A=30%, B=20% — both healthy
    let tick0_responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    let tick0 = score_from_responses(&tick0_responses, &profiles, false, 20.0, now);
    for (alias, score) in &tick0 {
        assert!(
            *score > 900.0,
            "{alias} should be usable at tick 0, got {score}"
        );
    }

    // Tick 1: A=60%, B=20%
    // Tick 2: A=90%, B=20%
    // Advance cursors by fetching 2 more times per account
    for _ in 0..2 {
        for (_alias, _path, token, _) in &profiles {
            let _ = client
                .get(server.usage_url())
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .unwrap();
        }
    }

    // Tick 3: A=100%, B=20% — A exhausted, B should win
    let tick3_responses = fetch_all(&client, &server.usage_url(), &profiles).await;
    // pool_exhausted is computed dynamically
    let tick3 = score_from_responses(&tick3_responses, &profiles, false, 20.0, now);

    assert_eq!(
        tick3[0].0, "gradual_b",
        "B should win when A is exhausted at tick 3"
    );
    assert!(tick3[1].1 < 500.0, "exhausted A should score low");

    server.shutdown();
}

#[tokio::test]
async fn http_mock_returns_correct_structure() {
    // Verify that the mock response is parseable by the real parse_usage
    let entries = scenarios::healthy_pool();
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(server.usage_url())
        .header("Authorization", "Bearer tok_healthy_a")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();

    // Verify structure matches real API
    assert!(body.get("plan_type").is_some(), "should have plan_type");
    assert!(body.get("rate_limit").is_some(), "should have rate_limit");
    assert!(
        body.pointer("/rate_limit/primary_window/used_percent")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/primary_window/reset_at")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/secondary_window/used_percent")
            .is_some()
    );
    assert!(
        body.pointer("/rate_limit/secondary_window/reset_at")
            .is_some()
    );
    assert!(body.get("credits").is_some(), "should have credits");

    // Parse through the real path
    let info = usage::parse_usage(&body);
    assert!(info.primary.is_some(), "should parse primary window");
    assert!(info.secondary.is_some(), "should parse secondary window");
    assert_eq!(info.primary.as_ref().unwrap().used_percent, Some(0.0));

    server.shutdown();
}

#[tokio::test]
async fn http_unknown_token_returns_401() {
    let entries = scenarios::healthy_pool();
    let server = mock::MockServer::start(entries).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(server.usage_url())
        .header("Authorization", "Bearer unknown_token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401, "unknown token should get 401");

    server.shutdown();
}
