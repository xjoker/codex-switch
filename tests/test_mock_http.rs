//! HTTP-level integration tests using the mock server.
//!
//! These tests start a real HTTP mock server, create temp profile directories
//! with fake auth.json files, and call the mock directly via reqwest to verify
//! the HTTP → parse → score pipeline.

mod mock;

use codex_switch::auth;
use codex_switch::jwt::AccountInfo;
use codex_switch::usage::{self, ScoredCandidate};
use mock::scenarios;
use serde_json::json;
use std::path::PathBuf;

static HTTP_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: String) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Create a temp directory with fake profile auth.json files.
/// Returns (temp_dir, vec of (alias, path, token, JWT account info)).
fn setup_profiles(
    entries: &[(String, Vec<serde_json::Value>)],
) -> (
    tempfile::TempDir,
    Vec<(String, PathBuf, String, AccountInfo)>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut profiles = Vec::new();

    for (token, _responses) in entries {
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

        profiles.push((alias, auth_path, token.clone(), AccountInfo::default()));
    }

    (dir, profiles)
}

/// Helper: fetch usage from mock, parse, build candidates, compute pool state, score, and rank.
/// Returns (alias, score) sorted best-first.
fn score_from_responses(
    responses: &[(String, serde_json::Value)],
    profiles: &[(String, PathBuf, String, AccountInfo)],
    team_priority: bool,
    safety_margin_7d: f64,
    now: i64,
) -> Vec<(String, f64)> {
    score_candidates_from_responses(responses, profiles, team_priority, safety_margin_7d, now)
        .into_iter()
        .map(|scored| (scored.candidate.alias, scored.score))
        .collect()
}

fn score_candidates_from_responses(
    responses: &[(String, serde_json::Value)],
    profiles: &[(String, PathBuf, String, AccountInfo)],
    team_priority: bool,
    safety_margin_7d: f64,
    now: i64,
) -> Vec<ScoredCandidate> {
    let inputs = responses
        .iter()
        .map(|(alias, body)| {
            let account = profiles
                .iter()
                .find(|(profile_alias, _, _, _)| profile_alias == alias)
                .unwrap()
                .3
                .clone();
            (alias.clone(), usage::parse_usage(body), account, 0)
        })
        .collect();
    let mut scored = usage::score_candidates(inputs, now, safety_margin_7d, team_priority);
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    scored
}

/// Fetch all profiles from mock server and return (alias, response_body) pairs.
async fn fetch_all(
    client: &reqwest::Client,
    url: &str,
    profiles: &[(String, PathBuf, String, AccountInfo)],
) -> Vec<(String, serde_json::Value)> {
    let mut results = Vec::new();
    for (alias, _path, token, _account) in profiles {
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

#[test]
fn score_candidates_pool_size_counts_successful_responses_not_profiles() {
    let entries = scenarios::healthy_pool();
    let (_dir, profiles) = setup_profiles(&entries);
    let responses: Vec<_> = entries
        .iter()
        .take(2)
        .map(|(token, bodies)| {
            (
                token.strip_prefix("tok_").unwrap_or(token).to_string(),
                bodies[0].clone(),
            )
        })
        .collect();

    let scored =
        score_candidates_from_responses(&responses, &profiles, false, 20.0, auth::now_unix_secs());

    assert_eq!(profiles.len(), 3);
    assert_eq!(scored.len(), 2);
    assert!(
        scored
            .iter()
            .all(|candidate| candidate.candidate.pool_size == 2)
    );
}

#[tokio::test]
async fn usage_401_refreshes_json_token_and_retries_with_new_access_token() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let refreshed_access_token = "mock_access_refresh_old";
    let server = mock::MockServer::start_programmed(vec![
        (
            "old_access".to_string(),
            vec![mock::MockResponse::json(
                reqwest::StatusCode::UNAUTHORIZED,
                json!({"error": "expired"}),
            )],
        ),
        (
            refreshed_access_token.to_string(),
            vec![mock::MockResponse::json(
                reqwest::StatusCode::OK,
                mock::transformer::base_response("plus", 12.0, 18000, 20.0, 604800),
            )],
        ),
    ])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());
    let _token_url = EnvVarGuard::set("CS_TOKEN_URL", server.token_url());
    let _reset_url = EnvVarGuard::remove("CS_RESET_CREDITS_URL");

    let outcome = usage::fetch_usage_with_refresh(
        "refresh_case",
        "old_access",
        Some("old_id"),
        Some("refresh_old"),
        None,
        false,
    )
    .await;
    let usage = outcome.result.unwrap();

    assert_eq!(usage.primary.unwrap().used_percent, Some(12.0));
    assert_eq!(
        outcome.refreshed.unwrap().access_token,
        refreshed_access_token
    );
    assert_eq!(server.request_count("old_access"), 1);
    assert_eq!(server.request_count(refreshed_access_token), 1);
    server.shutdown();
}

#[tokio::test]
async fn usage_5xx_returns_contextual_error() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "server_error".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream failed",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());

    let error =
        usage::fetch_usage_with_refresh("server_error", "server_error", None, None, None, false)
            .await
            .result
            .expect_err("HTTP 500 must fail");

    assert!(error.to_string().contains("HTTP 500"), "{error:#}");
    server.shutdown();
}

#[tokio::test]
async fn usage_malformed_json_returns_parse_context() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "malformed".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::OK,
            "not-json",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());

    let error = usage::fetch_usage_with_refresh("malformed", "malformed", None, None, None, false)
        .await
        .result
        .expect_err("malformed JSON must fail");

    assert!(
        error
            .to_string()
            .contains("failed to parse usage response (HTTP 200 OK)"),
        "{error:#}"
    );
    server.shutdown();
}

#[tokio::test]
async fn usage_retry_exhaustion_returns_last_error_after_three_attempts() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let server = mock::MockServer::start_programmed(vec![(
        "retry_exhausted".to_string(),
        vec![mock::MockResponse::text(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "still unavailable",
        )],
    )])
    .await;
    let _usage_url = EnvVarGuard::set("CS_USAGE_URL", server.usage_url());
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    std::fs::write(
        &auth_path,
        serde_json::to_vec(&json!({
            "tokens": {"access_token": "retry_exhausted"}
        }))
        .unwrap(),
    )
    .unwrap();

    let error = usage::fetch_usage_retried_force("retry_exhausted", &auth_path, "")
        .await
        .unwrap_err();

    assert!(error.detail.contains("HTTP 503"), "{}", error.detail);
    assert_eq!(server.request_count("retry_exhausted"), 3);
    server.shutdown();
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
async fn http_reset_card_consume_uses_earliest_expiry() {
    let _lock = HTTP_ENV_LOCK.lock().await;
    let entries = scenarios::healthy_pool();
    let (_dir, profiles) = setup_profiles(&entries);
    let server = mock::MockServer::start(entries).await;
    let (_alias, auth_path, _token, _is_team) = profiles
        .iter()
        .find(|(alias, _, _, _)| alias == "healthy_a")
        .unwrap();

    let _reset_url_guard = EnvVarGuard::set("CS_RESET_CREDITS_URL", server.reset_credits_url());
    let _consume_url_guard = EnvVarGuard::remove("CS_RESET_CREDITS_CONSUME_URL");

    let credit = usage::fetch_earliest_reset_credit("healthy_a", auth_path)
        .await
        .unwrap();
    let result = usage::consume_reset_credit_by_id("healthy_a", auth_path, &credit.id)
        .await
        .unwrap();

    assert_eq!(result.credit.id, "reset_credit_1");
    assert_eq!(result.windows_reset, Some(2));
    assert_eq!(result.code.as_deref(), Some("reset"));

    server.shutdown();
}

// ── reset-card-aware auto-switching (spawned binary, end-to-end) ──

mod revival {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::mock;
    use serde_json::{Value, json};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_home(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("codex-switch-revival-{name}-{ts}-{id}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn jwt(payload: &Value) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let json_bytes = serde_json::to_vec(payload).unwrap();
        format!("x.{}.y", URL_SAFE_NO_PAD.encode(json_bytes))
    }

    fn auth_json_with_access(email: &str, account_id: &str, access_token: &str) -> Value {
        let claims = json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
                "organizations": [],
            }
        });
        json!({
            "tokens": {
                "id_token": jwt(&claims),
                "refresh_token": "dummy-refresh",
                "access_token": access_token,
                "account_id": account_id,
            }
        })
    }

    fn write_json(path: impl AsRef<Path>, value: &Value) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Write a profile with a cached usage entry, bypassing any network fetch
    /// for the initial pool scan.
    fn write_cached_profile(
        home: &Path,
        alias: &str,
        access_token: &str,
        primary_used: f64,
        reset_credits: &[(&str, Option<&str>)],
    ) {
        write_json(
            home.join(format!(".codex-switch/profiles/{alias}/auth.json")),
            &auth_json_with_access(
                &format!("{alias}@mock.test"),
                &format!("acct_{alias}"),
                access_token,
            ),
        );

        let credits: Vec<Value> = reset_credits
            .iter()
            .map(|(id, expires_at)| {
                json!({
                    "id": id,
                    "granted_at": null,
                    "expires_at": expires_at,
                })
            })
            .collect();

        let now = now_secs();
        let cache_path = home.join(".codex-switch/cache.json");
        let mut cache: Value = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({"entries": {}}));
        cache["entries"][alias] = json!({
            "ts": now,
            "primary_used": primary_used,
            "primary_reset": now as i64 + 7200,
            "secondary_used": 10.0,
            "secondary_reset": now as i64 + 604800,
            "plan_type": "plus",
            "reset_credits_available_count": credits.len(),
            "reset_credits": credits,
        });
        write_json(&cache_path, &cache);
    }

    fn write_long_ttl_config(home: &Path) {
        fs::create_dir_all(home.join(".codex-switch")).unwrap();
        fs::write(
            home.join(".codex-switch/config.toml"),
            "[cache]\nttl = 999999999\n",
        )
        .unwrap();
    }

    fn binary() -> &'static str {
        env!("CARGO_BIN_EXE_codex-switch")
    }

    fn command(home: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(binary());
        cmd.args(args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.stdin(Stdio::null()); // never a TTY: CardPolicy::Prompt cannot trigger here
        cmd.env("HOME", home);
        cmd.env("CODEX_HOME", home.join(".codex"));
        cmd.env("CODEX_SWITCH_HOME", home.join(".codex-switch"));
        cmd.env_remove("HTTP_PROXY");
        cmd.env_remove("HTTPS_PROXY");
        cmd.env_remove("ALL_PROXY");
        cmd.env_remove("CS_PROXY");
        cmd
    }

    fn run_with_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = command(home, args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    fn parse_stdout_json(output: &Output) -> Value {
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Unreachable loopback address: any accidental network call fails fast
    /// instead of hanging or reaching a real host.
    const UNROUTABLE_URL: &str = "http://127.0.0.1:1/unused";

    async fn invalid_consume_url() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route("/consume", post(|| async { (StatusCode::OK, "not-json") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/consume"), server)
    }

    /// Contract 5: an eligible top candidate short-circuits card logic
    /// entirely -- the consume endpoint must never be reached, even though
    /// another account in the pool holds a reset card.
    #[test]
    fn contract5_eligible_top_skips_card_logic_and_never_hits_consume() {
        let home = temp_home("contract5");
        write_long_ttl_config(&home);
        write_cached_profile(&home, "eligible_top", "tok_eligible_top", 5.0, &[]);
        write_cached_profile(
            &home,
            "other_holds_card",
            "tok_other_holds_card",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );

        let output = run_with_env(
            &home,
            &["--json", "use"],
            &[
                ("CS_USAGE_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_CONSUME_URL", UNROUTABLE_URL),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = parse_stdout_json(&output);
        assert_eq!(json["switched_to"], "eligible_top");
        assert!(
            json.get("hint").is_none(),
            "no hint expected when top candidate is eligible: {json}"
        );

        let _ = fs::remove_dir_all(home);
    }

    /// Contract 6: pool exhausted, one account holds a card, PreApproved
    /// (--consume-card) -> consumes exactly one card and revives the account.
    ///
    /// Multi-thread runtime: the child process's blocking `Command::output()`
    /// call must not starve the MockServer's own async task on the same thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn contract6_consume_card_revives_exhausted_pool() {
        let home = temp_home("contract6");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[
                ("reset_credit_1", Some("2026-07-08T00:00:00Z")),
                ("reset_credit_2", Some("2026-07-09T00:00:00Z")),
            ],
        );

        // Real usage endpoint response used only for the post-consume force-refresh
        // (the initial scan is served entirely from cache).
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 0.0, 18000, 10.0, 604800,
            )],
        )];
        let server = mock::MockServer::start(entries).await;

        let output = run_with_env(
            &home,
            &["--json", "use", "--consume-card"],
            &[
                ("CS_USAGE_URL", &server.usage_url()),
                ("CS_RESET_CREDITS_URL", &server.reset_credits_url()),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = parse_stdout_json(&output);
        assert_eq!(json["switched_to"], "card_holder");
        assert!(
            json.get("hint").is_none(),
            "no hint expected after a successful revival: {json}"
        );
        assert_eq!(
            json.pointer("/usage/primary/used_percent"),
            Some(&serde_json::json!(0.0)),
            "revived usage should reflect the post-consume fetch: {json}"
        );

        server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    /// Contract 7: consuming the card doesn't actually free quota (still
    /// exhausted on recheck) -> falls back to the original scored candidate
    /// without trying a second card.
    ///
    /// Multi-thread runtime: see `contract6_consume_card_revives_exhausted_pool`.
    #[tokio::test(flavor = "multi_thread")]
    async fn contract7_falls_back_when_still_exhausted_after_consume() {
        let home = temp_home("contract7");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );

        // Post-consume force-refresh still reports 100% used.
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let server = mock::MockServer::start(entries).await;

        let output = run_with_env(
            &home,
            &["--json", "use", "--consume-card"],
            &[
                ("CS_USAGE_URL", &server.usage_url()),
                ("CS_RESET_CREDITS_URL", &server.reset_credits_url()),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = parse_stdout_json(&output);
        // Still exhausted -> falls back to the (only, still-exhausted) scored
        // candidate rather than erroring out.
        assert_eq!(json["switched_to"], "card_holder");
        let hint = json["hint"]
            .as_str()
            .expect("consumed card must be reported");
        assert!(hint.contains("card_holder"), "{hint}");
        assert!(hint.contains("card was consumed"), "{hint}");
        assert!(hint.contains("could not be confirmed revived"), "{hint}");

        server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn contract7_human_output_reports_consumed_but_unconfirmed_card() {
        let home = temp_home("contract7-human");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let server = mock::MockServer::start(entries).await;

        let output = run_with_env(
            &home,
            &["use", "--consume-card"],
            &[
                ("CS_USAGE_URL", &server.usage_url()),
                ("CS_RESET_CREDITS_URL", &server.reset_credits_url()),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("card_holder"), "{stdout}");
        assert!(stdout.contains("card was consumed"), "{stdout}");
        assert!(
            stdout.contains("could not be confirmed revived"),
            "{stdout}"
        );

        server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consume_unknown_json_output_warns_to_verify_before_retry() {
        let home = temp_home("consume-unknown-json");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let usage_server = mock::MockServer::start(entries).await;
        let (consume_url, consume_server) = invalid_consume_url().await;

        let output = run_with_env(
            &home,
            &["--json", "use", "--consume-card"],
            &[
                ("CS_USAGE_URL", &usage_server.usage_url()),
                ("CS_RESET_CREDITS_URL", &usage_server.reset_credits_url()),
                ("CS_RESET_CREDITS_CONSUME_URL", &consume_url),
            ],
        );

        assert!(output.status.success());
        let json = parse_stdout_json(&output);
        let hint = json["hint"]
            .as_str()
            .expect("unknown outcome must be visible");
        assert!(hint.contains("card_holder"), "{hint}");
        assert!(hint.contains("consumption may have occurred"), "{hint}");
        assert!(hint.contains("verify before retry"), "{hint}");

        consume_server.abort();
        usage_server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn consume_unknown_human_output_warns_to_verify_before_retry() {
        let home = temp_home("consume-unknown-human");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let usage_server = mock::MockServer::start(entries).await;
        let (consume_url, consume_server) = invalid_consume_url().await;

        let output = run_with_env(
            &home,
            &["use", "--consume-card"],
            &[
                ("CS_USAGE_URL", &usage_server.usage_url()),
                ("CS_RESET_CREDITS_URL", &usage_server.reset_credits_url()),
                ("CS_RESET_CREDITS_CONSUME_URL", &consume_url),
            ],
        );

        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("card_holder"), "{stdout}");
        assert!(stdout.contains("consumption may have occurred"), "{stdout}");
        assert!(stdout.contains("verify before retry"), "{stdout}");

        consume_server.abort();
        usage_server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_reset_card_unknown_json_warns_to_verify_before_retry() {
        let home = temp_home("explicit-reset-unknown-json");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let usage_server = mock::MockServer::start(entries).await;
        let (consume_url, consume_server) = invalid_consume_url().await;

        let output = run_with_env(
            &home,
            &["--json", "reset-card", "card_holder", "--yes"],
            &[
                ("CS_USAGE_URL", &usage_server.usage_url()),
                ("CS_RESET_CREDITS_URL", &usage_server.reset_credits_url()),
                ("CS_RESET_CREDITS_CONSUME_URL", &consume_url),
            ],
        );

        assert!(!output.status.success());
        let json = parse_stdout_json(&output);
        let message = json["error"].as_str().expect("JSON error must be present");
        assert!(message.contains("card_holder"), "{message}");
        assert!(
            message.contains("consumption may have occurred"),
            "{message}"
        );
        assert!(message.contains("verify before retry"), "{message}");

        consume_server.abort();
        usage_server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_reset_card_unknown_human_warns_to_verify_before_retry() {
        let home = temp_home("explicit-reset-unknown-human");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );
        let entries = vec![(
            "tok_card_holder".to_string(),
            vec![mock::transformer::base_response(
                "plus", 100.0, 1800, 50.0, 604800,
            )],
        )];
        let usage_server = mock::MockServer::start(entries).await;
        let (consume_url, consume_server) = invalid_consume_url().await;

        let output = run_with_env(
            &home,
            &["reset-card", "card_holder", "--yes"],
            &[
                ("CS_USAGE_URL", &usage_server.usage_url()),
                ("CS_RESET_CREDITS_URL", &usage_server.reset_credits_url()),
                ("CS_RESET_CREDITS_CONSUME_URL", &consume_url),
            ],
        );

        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("card_holder"), "{stderr}");
        assert!(stderr.contains("consumption may have occurred"), "{stderr}");
        assert!(stderr.contains("verify before retry"), "{stderr}");

        consume_server.abort();
        usage_server.shutdown();
        let _ = fs::remove_dir_all(home);
    }

    /// Contract 8: pool exhausted, a card is available, but no
    /// --consume-card flag and no TTY -> Deny. Nothing is consumed; both the
    /// JSON and human output carry the revival hint.
    #[test]
    fn contract8_deny_without_flag_emits_hint_and_consumes_nothing() {
        let home = temp_home("contract8-json");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[
                ("reset_credit_1", Some("2026-07-08T00:00:00Z")),
                ("reset_credit_2", Some("2026-07-09T00:00:00Z")),
            ],
        );

        let output = run_with_env(
            &home,
            &["--json", "use"],
            &[
                ("CS_USAGE_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_CONSUME_URL", UNROUTABLE_URL),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = parse_stdout_json(&output);
        assert_eq!(json["switched_to"], "card_holder");
        let hint = json["hint"].as_str().expect("hint field should be set");
        assert!(hint.contains("card_holder"), "{hint}");
        assert!(hint.contains("--consume-card"), "{hint}");

        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn contract8_deny_without_flag_prints_human_hint() {
        let home = temp_home("contract8-human");
        write_long_ttl_config(&home);
        write_cached_profile(
            &home,
            "card_holder",
            "tok_card_holder",
            100.0,
            &[("reset_credit_1", Some("2026-07-08T00:00:00Z"))],
        );

        let output = run_with_env(
            &home,
            &["use"],
            &[
                ("CS_USAGE_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_URL", UNROUTABLE_URL),
                ("CS_RESET_CREDITS_CONSUME_URL", UNROUTABLE_URL),
            ],
        );

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("card_holder"), "{stdout}");
        assert!(stdout.contains("--consume-card"), "{stdout}");

        let _ = fs::remove_dir_all(home);
    }
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
