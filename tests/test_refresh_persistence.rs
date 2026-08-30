//! Regression tests for OAuth refresh-token rotation safety.
//!
//! OpenAI rotates `refresh_token` on every use and rejects replays with
//! `refresh_token_reused`. Any path that obtains a new token but fails to
//! persist it — or that replays a consumed token — permanently bricks the
//! profile, so these behaviours are covered end-to-end against a local mock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

/// Env vars are process-global; serialize every test that touches them.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

#[derive(Clone)]
struct Reply {
    status: StatusCode,
    body: Value,
}

fn reply(status: StatusCode, body: Value) -> Reply {
    Reply { status, body }
}

fn rotation(n: u32) -> Reply {
    reply(
        StatusCode::OK,
        json!({
            "id_token": format!("id_{n}"),
            "access_token": format!("access_{n}"),
            "refresh_token": format!("refresh_{n}"),
        }),
    )
}

/// Simulates the process that *won* a concurrent rotation race.
///
/// The auth server only starts answering `refresh_token_reused` once it has
/// issued the winner's replacement, and the winner writes that replacement to
/// the profile. Performing the write inside the token handler reproduces that
/// ordering exactly, without any sleep-based coordination in the test.
#[derive(Clone)]
struct ConcurrentWinner {
    profile_path: PathBuf,
    id_token: String,
    access_token: String,
    refresh_token: String,
}

/// Parks every token request inside the handler until the test lets it go.
///
/// Slowness is expressed as an explicit signal, never as a sleep the test hopes
/// is long enough: `arrived` reports that the request has really reached the
/// auth server (so the rotation is already spent), and the response is only
/// produced once a permit is added to `release`.
#[derive(Clone)]
struct TokenGate {
    arrived: tokio::sync::mpsc::UnboundedSender<String>,
    release: Arc<tokio::sync::Semaphore>,
}

/// Test-side handle for the requests [`TokenGate`] is holding.
struct HeldTokenRequests {
    arrivals: tokio::sync::mpsc::UnboundedReceiver<String>,
    release: Arc<tokio::sync::Semaphore>,
}

impl HeldTokenRequests {
    /// Block until `n` refresh requests are parked in the handler. Returns the
    /// `refresh_token` each of them presented, in arrival order.
    async fn wait_for(&mut self, n: usize) -> Vec<String> {
        let mut seen = Vec::new();
        while seen.len() < n {
            let next = tokio::time::timeout(Duration::from_secs(10), self.arrivals.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("expected {n} token request(s), only {seen:?} arrived within 10s")
                })
                .expect("token endpoint gate closed");
            seen.push(next);
        }
        seen
    }

    /// Let every parked request — and any that arrives later — answer.
    fn release_all(&self) {
        self.release.add_permits(64);
    }
}

#[derive(Default)]
struct MockState {
    /// Usage replies keyed by bearer token; the last entry repeats.
    usage: HashMap<String, Vec<Reply>>,
    usage_cursor: HashMap<String, usize>,
    /// Bearer tokens seen by the usage endpoint, in order.
    usage_calls: Vec<String>,
    /// Token-endpoint replies; the last entry repeats.
    token_replies: Vec<Reply>,
    /// Token-endpoint replies keyed by the presented `refresh_token`. Takes
    /// precedence over `token_replies`, so tests whose profiles refresh
    /// concurrently do not depend on which one reaches the server first.
    token_by_refresh: HashMap<String, Reply>,
    /// `refresh_token` values seen by the token endpoint, in order.
    token_calls: Vec<String>,
    /// Keyed by the presented `refresh_token`: rewrite a profile with a
    /// concurrent winner's credentials *before* answering this request.
    winner_writes: HashMap<String, ConcurrentWinner>,
    /// When set, hold every token request until the test releases it.
    gate: Option<TokenGate>,
}

type SharedState = Arc<Mutex<MockState>>;

struct MockServer {
    addr: SocketAddr,
    state: SharedState,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl MockServer {
    async fn start(usage: Vec<(String, Vec<Reply>)>, token_replies: Vec<Reply>) -> Self {
        Self::start_with(usage, token_replies, HashMap::new()).await
    }

    /// Start with token replies chosen by the presented `refresh_token` rather
    /// than by call order.
    async fn start_keyed_by_refresh_token(token_by_refresh: Vec<(String, Reply)>) -> Self {
        Self::start_with(
            Vec::new(),
            Vec::new(),
            token_by_refresh.into_iter().collect(),
        )
        .await
    }

    async fn start_with(
        usage: Vec<(String, Vec<Reply>)>,
        token_replies: Vec<Reply>,
        token_by_refresh: HashMap<String, Reply>,
    ) -> Self {
        let state: SharedState = Arc::new(Mutex::new(MockState {
            usage: usage.into_iter().collect(),
            token_replies,
            token_by_refresh,
            ..Default::default()
        }));

        let app = Router::new()
            .route("/backend-api/wham/usage", get(usage_handler))
            .route("/oauth/token", post(token_handler))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        Self {
            addr,
            state,
            shutdown_tx,
        }
    }

    fn usage_url(&self) -> String {
        format!("http://{}/backend-api/wham/usage", self.addr)
    }

    fn token_url(&self) -> String {
        format!("http://{}/oauth/token", self.addr)
    }

    /// Register a concurrent winner: the next time the token endpoint is asked
    /// to rotate `presented`, `winner`'s credentials land in its profile first.
    fn set_concurrent_winner(&self, presented: &str, winner: ConcurrentWinner) {
        self.state
            .lock()
            .unwrap()
            .winner_writes
            .insert(presented.to_string(), winner);
    }

    /// Hold every token request until the returned handle releases it.
    fn hold_token_requests(&self) -> HeldTokenRequests {
        let (arrived, arrivals) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        self.state.lock().unwrap().gate = Some(TokenGate {
            arrived,
            release: release.clone(),
        });
        HeldTokenRequests { arrivals, release }
    }

    fn token_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().token_calls.clone()
    }

    fn usage_calls(&self) -> Vec<String> {
        self.state.lock().unwrap().usage_calls.clone()
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn next_reply(replies: &[Reply], cursor: usize) -> Reply {
    replies[cursor.min(replies.len() - 1)].clone()
}

async fn usage_handler(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();

    let mut guard = state.lock().unwrap();
    guard.usage_calls.push(bearer.clone());
    let Some(replies) = guard.usage.get(&bearer).cloned() else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"detail": format!("unknown token {bearer}")})),
        )
            .into_response();
    };
    let cursor = guard.usage_cursor.entry(bearer).or_insert(0);
    let chosen = next_reply(&replies, *cursor);
    *cursor += 1;
    (chosen.status, axum::Json(chosen.body)).into_response()
}

async fn token_handler(
    State(state): State<SharedState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let presented = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    // Record the call and pick the reply under the lock, then drop the guard —
    // a gated request parks on an await that must not hold a std mutex.
    let (chosen, gate) = {
        let mut guard = state.lock().unwrap();
        guard.token_calls.push(presented.clone());
        if let Some(winner) = guard.winner_writes.get(&presented).cloned() {
            write_auth_file(
                &winner.profile_path,
                &winner.id_token,
                &winner.access_token,
                &winner.refresh_token,
            );
        }
        let chosen = match guard.token_by_refresh.get(&presented) {
            Some(reply) => reply.clone(),
            None => {
                let cursor = guard.token_calls.len() - 1;
                next_reply(&guard.token_replies, cursor)
            }
        };
        (chosen, guard.gate.clone())
    };

    // The rotation is spent from here on: the server has "seen" the token, so
    // the client may no longer treat the request as cancellable.
    if let Some(gate) = gate {
        let _ = gate.arrived.send(presented);
        if let Ok(permit) = gate.release.acquire().await {
            permit.forget();
        }
    }

    (chosen.status, axum::Json(chosen.body)).into_response()
}

/// A JWT whose `exp` is `secs` from now (negative = already expired).
fn jwt_expiring_in(secs: i64) -> String {
    let exp = codex_switch::auth::now_unix_secs() + secs;
    let payload = URL_SAFE_NO_PAD.encode(json!({"exp": exp}).to_string());
    format!("header.{payload}.signature")
}

fn expired_jwt() -> String {
    jwt_expiring_in(-3600)
}

fn write_auth_file(path: &Path, id: &str, access: &str, refresh: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id,
                "access_token": access,
                "refresh_token": refresh,
            },
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_profile(home: &Path, alias: &str, id: &str, access: &str, refresh: &str) -> PathBuf {
    let path = home.join("profiles").join(alias).join("auth.json");
    write_auth_file(&path, id, access, refresh);
    path
}

fn stored_refresh_token(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap();
    let val: Value = serde_json::from_str(&raw).unwrap();
    val.pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Env + profile fixture. Field order matters: guards drop before `home`.
struct Fixture {
    profile_path: PathBuf,
    _guards: Vec<EnvVarGuard>,
    _home: tempfile::TempDir,
}

fn env_guards(server: &MockServer, home: &Path) -> Vec<EnvVarGuard> {
    vec![
        EnvVarGuard::set("CODEX_SWITCH_HOME", home.display().to_string()),
        EnvVarGuard::set("CODEX_HOME", home.join("codex").display().to_string()),
        EnvVarGuard::set("CS_USAGE_URL", server.usage_url()),
        EnvVarGuard::set("CS_TOKEN_URL", server.token_url()),
        EnvVarGuard::remove("CS_RESET_CREDITS_URL"),
    ]
}

fn fixture(server: &MockServer, alias: &str, access_token: &str) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let guards = env_guards(server, home.path());
    let profile_path = write_profile(home.path(), alias, "old_id", access_token, "refresh_old");
    Fixture {
        profile_path,
        _guards: guards,
        _home: home,
    }
}

/// Same as [`fixture`], except the profile's own `auth.json` — the path the
/// persist step derives from the alias — is occupied by a *directory*, so every
/// attempt to save rotated tokens fails deterministically on unix and Windows
/// alike (no permission-bit semantics involved).
///
/// The tokens the fetch starts from are staged in a separate readable file, so
/// the run reaches a successful refresh first. That reproduces the production
/// window this guards: the profile is read fine, the auth server then rotates
/// the credentials, and only the write back fails (disk full, permissions
/// revoked, path clobbered).
fn fixture_with_unwritable_profile(
    server: &MockServer,
    alias: &str,
    access_token: &str,
) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let guards = env_guards(server, home.path());
    let profile_path = home.path().join("staged").join("auth.json");
    write_auth_file(&profile_path, "old_id", access_token, "refresh_old");
    std::fs::create_dir_all(home.path().join("profiles").join(alias).join("auth.json")).unwrap();
    Fixture {
        profile_path,
        _guards: guards,
        _home: home,
    }
}

fn usage_ok() -> Reply {
    reply(
        StatusCode::OK,
        json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.5,
                    "limit_window_seconds": 18_000,
                    "reset_at": codex_switch::auth::now_unix_secs() + 3_600,
                }
            }
        }),
    )
}

fn reused_refresh_reply() -> Reply {
    reply(
        StatusCode::UNAUTHORIZED,
        json!({
            "error": {
                "code": "refresh_token_reused",
                "message": "Your refresh token has already been used to generate a new access token. Please try signing in again.",
                "param": null,
                "type": "invalid_request_error",
            }
        }),
    )
}

/// D1: a rotated refresh_token is single-use. If we obtain one and then drop it
/// because the follow-up usage call failed, the profile can never authenticate
/// again — so it must reach disk regardless of what happens afterwards.
#[tokio::test]
async fn rotated_refresh_token_is_persisted_even_when_usage_fails_afterwards() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![
            (
                "old_access".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "access token expired"}),
                )],
            ),
            (
                "access_1".to_string(),
                vec![reply(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"detail": "upstream exploded"}),
                )],
            ),
        ],
        vec![rotation(1)],
    )
    .await;
    let fx = fixture(&server, "team1", "old_access");

    let err = codex_switch::usage::fetch_usage_retried_force("team1", &fx.profile_path, "team1")
        .await
        .expect_err("usage must fail in this scenario");

    assert_eq!(
        stored_refresh_token(&fx.profile_path),
        "refresh_1",
        "refresh token rotated by the auth server was lost; profile is now bricked (error was: {err})"
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "the consumed refresh token must never be replayed"
    );
    server.shutdown();
}

/// The usage path owns proactive rotation for the daemon, and a later 429
/// must not delay persisting the only valid replacement refresh token.
#[tokio::test]
async fn thirty_minute_rotation_is_persisted_before_usage_rate_limit_return() {
    let _lock = ENV_LOCK.lock().await;
    let expiring_access = jwt_expiring_in(10 * 60);
    let server = MockServer::start(
        vec![(
            "access_1".to_string(),
            vec![reply(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"detail": "slow down"}),
            )],
        )],
        vec![rotation(1)],
    )
    .await;
    let fx = fixture(&server, "early-persist", &expiring_access);
    let profile_path = fx.profile_path.clone();
    let fetch_path = profile_path.clone();

    let fetch = tokio::spawn(async move {
        codex_switch::usage::fetch_usage_retried_force(
            "early-persist",
            &fetch_path,
            "early-persist",
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.usage_calls().iter().any(|token| token == "access_1") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-refresh Usage request must reach the server");

    assert_eq!(
        stored_refresh_token(&profile_path),
        "refresh_1",
        "the rotated token must reach disk before the Usage 429 is returned"
    );

    fetch.abort();
    let _ = fetch.await;
    server.shutdown();
}

/// D2: once a refresh succeeds the old refresh_token is dead server-side.
/// Later retry rounds must present the rotated token, otherwise a transient
/// usage failure escalates into a permanent `refresh_token_reused` lockout.
#[tokio::test]
async fn each_retry_round_presents_the_rotated_refresh_token() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![
            (
                "old_access".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (
                "access_1".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (
                "access_2".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (
                "access_3".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
        ],
        vec![rotation(1), rotation(2), rotation(3)],
    )
    .await;
    let fx = fixture(&server, "team2", "old_access");

    let _ =
        codex_switch::usage::fetch_usage_retried_force("team2", &fx.profile_path, "team2").await;

    assert_eq!(
        server.token_calls(),
        vec![
            "refresh_old".to_string(),
            "refresh_1".to_string(),
            "refresh_2".to_string(),
        ],
        "retries replayed an already-consumed refresh token"
    );
    assert_eq!(
        server.usage_calls(),
        vec![
            "old_access".to_string(),
            "access_1".to_string(),
            "access_1".to_string(),
            "access_2".to_string(),
            "access_2".to_string(),
            "access_3".to_string(),
        ],
        "retries must carry the refreshed access token"
    );
    server.shutdown();
}

/// D3: OpenAI returns `error` as an object, not the OAuth-standard string.
/// The actionable server message must survive deserialization instead of being
/// replaced by a serde type error.
#[tokio::test]
async fn object_shaped_oauth_error_is_reported_with_code_and_message() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![(
            "old_access".to_string(),
            vec![reply(StatusCode::UNAUTHORIZED, json!({"detail": "expired"}))],
        )],
        vec![reply(
            StatusCode::UNAUTHORIZED,
            json!({
                "error": {
                    "code": "refresh_token_reused",
                    "message": "Your refresh token has already been used to generate a new access token. Please try signing in again.",
                    "param": null,
                    "type": "invalid_request_error",
                }
            }),
        )],
    )
    .await;
    let fx = fixture(&server, "team3", "old_access");

    let err = codex_switch::usage::fetch_usage_retried_force("team3", &fx.profile_path, "team3")
        .await
        .expect_err("a rejected refresh token must fail");

    assert!(
        err.detail.contains("refresh_token_reused"),
        "server error code missing from user-facing detail: {}",
        err.detail
    );
    assert!(
        err.detail.contains("Please try signing in again."),
        "server error message missing from user-facing detail: {}",
        err.detail
    );
    server.shutdown();
}

/// D3/D4: the OAuth-standard string shape must keep working too.
#[tokio::test]
async fn string_shaped_oauth_error_is_reported_with_description() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![(
            "old_access".to_string(),
            vec![reply(
                StatusCode::UNAUTHORIZED,
                json!({"detail": "expired"}),
            )],
        )],
        vec![reply(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "invalid_grant",
                "error_description": "The refresh token is invalid or has expired.",
            }),
        )],
    )
    .await;
    let fx = fixture(&server, "team5", "old_access");

    let err = codex_switch::usage::fetch_usage_retried_force("team5", &fx.profile_path, "team5")
        .await
        .expect_err("an invalid_grant refresh must fail");

    assert!(
        err.detail.contains("invalid_grant"),
        "server error code missing from user-facing detail: {}",
        err.detail
    );
    assert!(
        err.detail
            .contains("The refresh token is invalid or has expired."),
        "server error description missing from user-facing detail: {}",
        err.detail
    );
    server.shutdown();
}

/// D4: `refresh_token_reused` is terminal. Retrying burns wall-clock time on a
/// slow proxy and cannot succeed, so the auth endpoint must be hit exactly once
/// — including no second attempt inside the same round after a failed
/// proactive refresh.
#[tokio::test]
async fn reused_refresh_token_stops_retrying_after_a_single_auth_request() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let server = MockServer::start(
        vec![(
            stale_access.clone(),
            vec![reply(StatusCode::UNAUTHORIZED, json!({"detail": "expired"}))],
        )],
        vec![reply(
            StatusCode::UNAUTHORIZED,
            json!({
                "error": {
                    "code": "refresh_token_reused",
                    "message": "Your refresh token has already been used to generate a new access token. Please try signing in again.",
                    "param": null,
                    "type": "invalid_request_error",
                }
            }),
        )],
    )
    .await;
    let fx = fixture(&server, "team4", &stale_access);

    let err = codex_switch::usage::fetch_usage_retried_force("team4", &fx.profile_path, "team4")
        .await
        .expect_err("a reused refresh token must fail");

    assert_eq!(
        server.token_calls().len(),
        1,
        "terminal auth failure must not be retried, saw {:?} (error: {})",
        server.token_calls(),
        err.detail
    );
    assert!(
        err.summary.contains("refresh_token_reused"),
        "short summary must name the terminal auth failure: {}",
        err.summary
    );
    server.shutdown();
}

/// D5: the last data-loss window. Once the auth server has rotated the
/// credentials the previous refresh_token is dead, so a failed write leaves the
/// profile holding a token nothing will ever accept again. Reporting success
/// (the usage call itself worked) hands the user a bricked account that only
/// shows up at the next start — the failure has to surface now, and has to be
/// distinguishable from the auth server *rejecting* the refresh.
#[tokio::test]
async fn refresh_that_cannot_be_saved_fails_the_account_instead_of_reporting_success() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![
            (
                "old_access".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            ("access_1".to_string(), vec![usage_ok()]),
        ],
        vec![rotation(1)],
    )
    .await;
    let fx = fixture_with_unwritable_profile(&server, "team6", "old_access");

    let err = codex_switch::usage::fetch_usage_retried_force("team6", &fx.profile_path, "team6")
        .await
        .expect_err("a rotated token that never reached disk must not be reported as success");

    assert!(
        err.summary.contains("not saved"),
        "short summary must say the refreshed token was not saved: {}",
        err.summary
    );
    assert!(
        err.detail.contains("could not be saved"),
        "detail must state that saving the rotated credentials failed: {}",
        err.detail
    );
    assert!(
        err.detail.contains("sign in again"),
        "detail must warn that the account may need a new login: {}",
        err.detail
    );
    assert!(
        err.detail.contains("reading") && err.detail.contains("auth.json"),
        "detail must carry the underlying IO/permission cause: {}",
        err.detail
    );
    assert!(
        !err.detail.contains("token refresh rejected"),
        "a write failure must not read like an auth-server rejection: {}",
        err.detail
    );
    server.shutdown();
}

/// D5b: every refresh consumes a rotation the profile cannot get back. If the
/// write fails there is no reason to believe the next one will succeed, so the
/// account must stop immediately instead of spending further single-use tokens
/// on the same doomed round trip.
#[tokio::test]
async fn refresh_that_cannot_be_saved_does_not_burn_another_rotation() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![
            (
                "old_access".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (
                "access_1".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (
                "access_2".to_string(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
        ],
        vec![rotation(1), rotation(2), rotation(3)],
    )
    .await;
    let fx = fixture_with_unwritable_profile(&server, "team7", "old_access");

    let _ =
        codex_switch::usage::fetch_usage_retried_force("team7", &fx.profile_path, "team7").await;

    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "a token that could not be saved must not be followed by another rotation"
    );
    assert_eq!(
        server.usage_calls(),
        vec!["old_access".to_string()],
        "the account must not make a follow-up Usage request before the rotated token is durable"
    );
    server.shutdown();
}

/// Two profiles whose access tokens are already past expiry, so opportunistic
/// refresh picks both up. `blocked` is the profile in use, and its live
/// `$CODEX_HOME/auth.json` is occupied by a directory — the profile copy is
/// written, the live copy cannot be, and Codex would go on presenting a token
/// the auth server just invalidated.
struct OpportunisticFixture {
    keeper_profile: PathBuf,
    _guards: Vec<EnvVarGuard>,
    _home: tempfile::TempDir,
}

fn opportunistic_fixture(server: &MockServer) -> OpportunisticFixture {
    let home = tempfile::tempdir().unwrap();
    let guards = env_guards(server, home.path());
    let keeper_profile = write_profile(
        home.path(),
        "keeper",
        "old_id",
        &expired_jwt(),
        "refresh_keeper",
    );
    write_profile(
        home.path(),
        "blocked",
        "old_id",
        &expired_jwt(),
        "refresh_blocked",
    );
    std::fs::write(home.path().join("current"), "blocked").unwrap();
    std::fs::create_dir_all(home.path().join("codex").join("auth.json")).unwrap();
    OpportunisticFixture {
        keeper_profile,
        _guards: guards,
        _home: home,
    }
}

fn opportunistic_server_replies() -> Vec<(String, Reply)> {
    vec![
        (
            "refresh_keeper".to_string(),
            reply(
                StatusCode::OK,
                json!({
                    "id_token": "id_keeper_new",
                    "access_token": "access_keeper_new",
                    "refresh_token": "refresh_keeper_new",
                }),
            ),
        ),
        (
            "refresh_blocked".to_string(),
            reply(
                StatusCode::OK,
                json!({
                    "id_token": "id_blocked_new",
                    "access_token": "access_blocked_new",
                    "refresh_token": "refresh_blocked_new",
                }),
            ),
        ),
    ]
}

/// D7: opportunistic refresh spends the same single-use rotation as any other
/// refresh, and the daemon runs it on a timer. A write that fails there is a
/// silently bricked account with no trace the user can act on, so the failure
/// has to reach the caller instead of dying in a log line.
#[tokio::test]
async fn opportunistic_refresh_reports_the_profile_whose_token_could_not_be_saved() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start_keyed_by_refresh_token(opportunistic_server_replies()).await;
    let _fx = opportunistic_fixture(&server);

    let failures = codex_switch::usage::refresh_expiring_tokens().await;

    assert_eq!(
        failures
            .iter()
            .map(|f| f.alias.as_str())
            .collect::<Vec<_>>(),
        vec!["blocked"],
        "the profile whose rotated token could not be saved must be reported"
    );
    let detail = &failures[0].error.detail;
    assert!(
        detail.contains("could not be saved"),
        "detail must state that saving the rotated credentials failed: {detail}"
    );
    assert!(
        detail.contains("reading") && detail.contains("auth.json"),
        "detail must carry the underlying IO/permission cause: {detail}"
    );
    assert!(
        failures[0].error.summary.contains("not saved"),
        "summary must stay usable as a short status line: {}",
        failures[0].error.summary
    );
    server.shutdown();
}

/// D7b: opportunistic refresh is a batch. One profile that cannot be written
/// must not cost the others their refresh — they would each keep an expiring
/// token and hit the same cliff later.
#[tokio::test]
async fn opportunistic_refresh_keeps_going_after_one_profile_fails_to_save() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start_keyed_by_refresh_token(opportunistic_server_replies()).await;
    let fx = opportunistic_fixture(&server);

    let failures = codex_switch::usage::refresh_expiring_tokens().await;

    assert_eq!(
        stored_refresh_token(&fx.keeper_profile),
        "refresh_keeper_new",
        "a sibling profile must still be refreshed and saved (failures: {:?})",
        failures
            .iter()
            .map(|f| f.alias.as_str())
            .collect::<Vec<_>>()
    );
    let mut seen = server.token_calls();
    seen.sort();
    assert_eq!(
        seen,
        vec!["refresh_blocked".to_string(), "refresh_keeper".to_string()],
        "both expiring profiles must get their refresh attempt"
    );
    server.shutdown();
}

/// Writable profiles, all already past expiry so opportunistic refresh picks
/// every one of them up. Expiry increases with the position in `aliases`, which
/// is the order the batch starts them in (soonest first).
struct ExpiringFixture {
    profiles: Vec<PathBuf>,
    _guards: Vec<EnvVarGuard>,
    _home: tempfile::TempDir,
}

fn expiring_profiles_fixture(server: &MockServer, aliases: &[&str]) -> ExpiringFixture {
    let home = tempfile::tempdir().unwrap();
    let guards = env_guards(server, home.path());
    let profiles = aliases
        .iter()
        .enumerate()
        .map(|(index, alias)| {
            write_profile(
                home.path(),
                alias,
                "old_id",
                &jwt_expiring_in(-300 + index as i64 * 100),
                &format!("refresh_{alias}"),
            )
        })
        .collect();
    ExpiringFixture {
        profiles,
        _guards: guards,
        _home: home,
    }
}

/// The rotation the auth server hands back for `refresh_<alias>`.
fn rotation_for(alias: &str) -> (String, Reply) {
    (
        format!("refresh_{alias}"),
        reply(
            StatusCode::OK,
            json!({
                "id_token": format!("id_{alias}_new"),
                "access_token": format!("access_{alias}_new"),
                "refresh_token": format!("refresh_{alias}_new"),
            }),
        ),
    )
}

/// D12: the start budget must never cancel a rotation that is already in
/// flight. The moment the request reaches the auth server the presented
/// `refresh_token` is dead and its replacement exists only in that one
/// response, so dropping the task (which `JoinSet::drop` does, by aborting it)
/// leaves the profile holding a credential nothing will ever accept again.
/// A slow answer therefore still has to be read and written.
#[tokio::test]
async fn refresh_slower_than_the_budget_still_reaches_disk() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start_keyed_by_refresh_token(vec![rotation_for("slow")]).await;
    let fx = expiring_profiles_fixture(&server, &["slow"]);
    let mut held = server.hold_token_requests();
    let budget = Duration::from_millis(200);

    let (failures, ()) = tokio::join!(
        codex_switch::usage::refresh_expiring_tokens_within(budget),
        async {
            // The request is parked inside the handler, so the rotation is
            // already spent. Only then let the budget run out — the reply can
            // physically not be observed before that point.
            held.wait_for(1).await;
            tokio::time::sleep(budget * 3).await;
            held.release_all();
        }
    );

    assert_eq!(
        stored_refresh_token(&fx.profiles[0]),
        "refresh_slow_new",
        "a rotation that outlived the start budget was abandoned; the profile is \
         left holding a refresh token the auth server has already invalidated \
         (failures: {:?})",
        failures
            .iter()
            .map(|f| f.alias.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        failures.is_empty(),
        "the late rotation saved fine, so nothing should be reported: {:?}",
        failures
            .iter()
            .map(|f| f.alias.as_str())
            .collect::<Vec<_>>()
    );
    server.shutdown();
}

/// D13: the budget bounds how long the batch keeps *opening* new rotations, not
/// how long it waits for the open ones. Once it is spent, the candidates that
/// were never started must not be contacted at all: a request that is sent and
/// then abandoned is precisely the loss this design exists to prevent, and a
/// profile nobody contacted keeps a perfectly usable token for the next run.
#[tokio::test]
async fn budget_exhaustion_stops_starting_new_refreshes() {
    let _lock = ENV_LOCK.lock().await;
    let aliases = ["first", "second", "third"];
    let server = MockServer::start_keyed_by_refresh_token(
        aliases.iter().map(|alias| rotation_for(alias)).collect(),
    )
    .await;
    let fx = expiring_profiles_fixture(&server, &aliases);
    let mut held = server.hold_token_requests();
    let budget = Duration::from_millis(200);

    let (_failures, ()) = tokio::join!(
        codex_switch::usage::refresh_expiring_tokens_within(budget),
        async {
            // Every in-flight slot is occupied and stays occupied until the
            // budget is gone, so no further candidate may be started.
            held.wait_for(2).await;
            tokio::time::sleep(budget * 3).await;
            held.release_all();
        }
    );

    let mut seen = server.token_calls();
    seen.sort();
    assert_eq!(
        seen,
        vec!["refresh_first".to_string(), "refresh_second".to_string()],
        "a refresh was started after the budget was spent; every request sent is \
         a rotation that can be lost"
    );
    assert_eq!(
        stored_refresh_token(&fx.profiles[2]),
        "refresh_third",
        "the candidate that was never contacted must keep the token it had"
    );
    assert_eq!(
        stored_refresh_token(&fx.profiles[0]),
        "refresh_first_new",
        "the rotations that were started must still be saved"
    );
    assert_eq!(
        stored_refresh_token(&fx.profiles[1]),
        "refresh_second_new",
        "the rotations that were started must still be saved"
    );
    server.shutdown();
}

/// D14: none of the above may cost the ordinary case. When the auth server
/// answers promptly, every expiring profile is still refreshed and saved within
/// the production budget, even though only a bounded number run at a time.
#[tokio::test]
async fn every_expiring_profile_is_refreshed_when_the_server_answers_promptly() {
    let _lock = ENV_LOCK.lock().await;
    let aliases = ["alpha", "beta", "gamma"];
    let server = MockServer::start_keyed_by_refresh_token(
        aliases.iter().map(|alias| rotation_for(alias)).collect(),
    )
    .await;
    let fx = expiring_profiles_fixture(&server, &aliases);

    let failures = codex_switch::usage::refresh_expiring_tokens().await;

    assert!(
        failures.is_empty(),
        "no profile was unwritable: {:?}",
        failures
            .iter()
            .map(|f| f.alias.as_str())
            .collect::<Vec<_>>()
    );
    for (index, alias) in aliases.iter().enumerate() {
        assert_eq!(
            stored_refresh_token(&fx.profiles[index]),
            format!("refresh_{alias}_new"),
            "{alias} was not refreshed"
        );
    }
    let mut seen = server.token_calls();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            "refresh_alpha".to_string(),
            "refresh_beta".to_string(),
            "refresh_gamma".to_string(),
        ],
        "every expiring profile must get exactly one refresh attempt"
    );
    server.shutdown();
}

/// D8: `import` refreshes the credential *before* it validates usage, and the
/// auth value it mutates is a local of the caller. If the rotated tokens are
/// not handed back alongside the failure, the only credential the auth server
/// still accepts dies with that local — the imported account is unrecoverable.
#[tokio::test]
async fn import_validation_hands_back_rotated_tokens_when_usage_fails() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start(
        vec![(
            "access_1".to_string(),
            vec![reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"detail": "upstream exploded"}),
            )],
        )],
        vec![rotation(1)],
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let _guards = env_guards(&server, home.path());

    let mut val = json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "old_id",
            "access_token": expired_jwt(),
            "refresh_token": "refresh_old",
            "account_id": "acct_import",
        },
    });

    let outcome = codex_switch::usage::validate_import_auth(&mut val).await;

    assert!(
        outcome.result.is_err(),
        "the usage call must fail in this scenario"
    );
    let refreshed = outcome
        .refreshed
        .expect("rotated credentials must survive the failed validation");
    assert_eq!(
        refreshed.refresh_token, "refresh_1",
        "the caller needs the token the auth server just issued"
    );
    assert_eq!(
        val.pointer("/tokens/refresh_token")
            .and_then(|v| v.as_str()),
        Some("refresh_1"),
        "the auth value handed back to the caller must carry the rotated token"
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "the consumed refresh token must never be replayed"
    );
    server.shutdown();
}

/// D9: the same profile is refreshed concurrently — the daemon timer and a CLI
/// `list` both read RT1 and both present it. The auth server hands the rotation
/// to one of them and answers the other `refresh_token_reused`. That loser is
/// looking at a perfectly healthy account whose live credentials are already on
/// disk, so concluding "re-login required" costs the user a browser round trip
/// for nothing. A rejection must be re-checked against the profile before it is
/// believed.
#[tokio::test]
async fn refresh_rejected_by_a_concurrent_winner_recovers_from_the_stored_token() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let server = MockServer::start_with(
        vec![("access_winner".to_string(), vec![usage_ok()])],
        Vec::new(),
        HashMap::from([("refresh_old".to_string(), reused_refresh_reply())]),
    )
    .await;
    let fx = fixture(&server, "team8", &stale_access);
    server.set_concurrent_winner(
        "refresh_old",
        ConcurrentWinner {
            profile_path: fx.profile_path.clone(),
            id_token: "id_winner".to_string(),
            access_token: "access_winner".to_string(),
            refresh_token: "refresh_winner".to_string(),
        },
    );

    let usage = codex_switch::usage::fetch_usage_retried_force("team8", &fx.profile_path, "team8")
        .await
        .unwrap_or_else(|err| {
            panic!(
                "a profile whose token another process just rotated is healthy, \
                 but it was reported as: {} / {}",
                err.summary, err.detail
            )
        });

    assert_eq!(
        usage.primary.as_ref().and_then(|w| w.used_percent),
        Some(12.5),
        "the retry must return the account's real usage"
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "the stored token is already fresh; refreshing it again would burn a rotation"
    );
    assert_eq!(
        stored_refresh_token(&fx.profile_path),
        "refresh_winner",
        "the winner's rotation must be left untouched"
    );
    server.shutdown();
}

/// D10: the guard for D9 must not swallow real terminal failures. When the
/// profile on disk still holds the very token the auth server just rejected,
/// nobody rotated anything and the account genuinely needs a new login — that
/// verdict has to survive, and must not turn into extra rounds.
#[tokio::test]
async fn refresh_rejected_with_an_unchanged_profile_still_requires_a_new_login() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let server = MockServer::start_with(
        Vec::new(),
        Vec::new(),
        HashMap::from([("refresh_old".to_string(), reused_refresh_reply())]),
    )
    .await;
    let fx = fixture(&server, "team9", &stale_access);

    let err = codex_switch::usage::fetch_usage_retried_force("team9", &fx.profile_path, "team9")
        .await
        .expect_err("an unchanged profile means the rejection is real");

    assert!(
        err.summary.contains("re-login required"),
        "a genuinely dead credential must still ask for a new login: {}",
        err.summary
    );
    assert!(
        err.detail.contains("refresh_token_reused"),
        "the server verdict must stay visible: {}",
        err.detail
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "re-reading the profile costs no round trip; a dead token must not be replayed"
    );
    assert_eq!(
        stored_refresh_token(&fx.profile_path),
        "refresh_old",
        "nothing rotated, so the profile must be left as it was"
    );
    server.shutdown();
}

/// D11: the D9 recovery reads state two processes are both writing, so it must
/// not be able to feed itself. If the token adopted from disk is *also* rejected
/// because a third rotation landed meanwhile, the account has to stop rather
/// than chase the profile forever — otherwise two peers can keep re-arming each
/// other and neither ever reports anything.
///
/// **The verdict asserted below is a deliberate trade-off, not a healthy
/// outcome.** The account in this scenario is fine: some other process holds a
/// working credential and keeps rotating it. Bounding recovery to a single
/// retry means that in this case we report `re-login required` about an account
/// that does not need one — a false alarm that costs the user a pointless
/// browser round trip.
///
/// It is chosen over the alternative because the alternative is unbounded: a
/// recovery that re-arms itself every time the profile changes on disk can be
/// driven forever by a peer that keeps rotating, and then *nothing* is ever
/// reported and the round trips never stop. A bounded false alarm is visible
/// and recoverable; a livelock is neither. Reaching this state also needs three
/// rotations to land inside one call — two peers racing the same profile while
/// a third rotation slips in between — which the CLI and daemon timings make
/// vanishingly rare.
///
/// So: do not "fix" this by widening the retry budget, and do not read the
/// assertion below as "reporting a healthy account as dead is correct". If the
/// false alarm ever shows up in practice, the fix is to make the retry
/// distinguish *whose* rotation it observed (e.g. verify the adopted token
/// before condemning the account), not to let recovery loop.
#[tokio::test]
async fn concurrent_rotation_recovery_is_granted_at_most_once() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let server = MockServer::start_with(
        Vec::new(),
        Vec::new(),
        HashMap::from([
            ("refresh_old".to_string(), reused_refresh_reply()),
            ("refresh_w1".to_string(), reused_refresh_reply()),
            ("refresh_w2".to_string(), reused_refresh_reply()),
        ]),
    )
    .await;
    let fx = fixture(&server, "team10", &stale_access);
    for (presented, next) in [("refresh_old", "refresh_w1"), ("refresh_w1", "refresh_w2")] {
        server.set_concurrent_winner(
            presented,
            ConcurrentWinner {
                profile_path: fx.profile_path.clone(),
                id_token: "id_winner".to_string(),
                access_token: stale_access.clone(),
                refresh_token: next.to_string(),
            },
        );
    }

    let err = codex_switch::usage::fetch_usage_retried_force("team10", &fx.profile_path, "team10")
        .await
        .expect_err("every refresh in this scenario is rejected");

    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string(), "refresh_w1".to_string()],
        "recovery must be granted once, not once per rotation the profile picks up"
    );
    assert!(
        err.summary.contains("re-login required"),
        "recovery is bounded to one retry, so this run stops here and reports the \
         server's last verdict — a known false alarm for this account, accepted \
         because the unbounded alternative livelocks (see the comment above). \
         Reaching this line is the trade-off working as designed, not evidence \
         that the account is dead: {}",
        err.summary
    );
    server.shutdown();
}

// ── remembering a verdict the server already gave ────────────────────────
//
// Stopping the retry loop bounds the cost of a dead credential *within* one
// invocation. It does nothing across invocations: `list` and every TUI refresh
// re-present the same consumed token and wait for the same rejection, which on
// a slow path to the auth server is where the wall clock actually goes.

/// Drive one fetch against a token endpoint that answers `code`, then a second
/// fetch with everything unchanged. Returns `(first error, second error)` and
/// the mock, so callers can assert on what crossed the wire in between.
async fn two_fetches_against_rejected_credential(
    alias: &'static str,
    status: StatusCode,
    code: &str,
) -> (
    MockServer,
    codex_switch::usage::UsageError,
    codex_switch::usage::UsageError,
    Fixture,
) {
    let stale_access = expired_jwt();
    let server = MockServer::start(
        vec![(
            stale_access.clone(),
            vec![reply(
                StatusCode::UNAUTHORIZED,
                json!({"detail": "expired"}),
            )],
        )],
        vec![reply(
            status,
            json!({
                "error": {
                    "code": code,
                    "message": "Please try signing in again.",
                    "param": null,
                    "type": "invalid_request_error",
                }
            }),
        )],
    )
    .await;
    let fx = fixture(&server, alias, &stale_access);

    let first = codex_switch::usage::fetch_usage_retried(alias, &fx.profile_path, alias)
        .await
        .expect_err("the auth server rejected the credential");
    let second = codex_switch::usage::fetch_usage_retried(alias, &fx.profile_path, alias)
        .await
        .expect_err("nothing changed, so the account is still unusable");

    (server, first, second, fx)
}

/// `refresh_token_reused` is the auth server stating the credential is spent.
/// That verdict cannot change while the profile still holds the same token, so
/// asking again is pure latency — and on a slow network it is *most* of the
/// latency a user experiences from `list`.
#[tokio::test]
async fn a_reused_verdict_is_not_re_presented_on_the_next_invocation() {
    let _lock = ENV_LOCK.lock().await;
    let (server, first, second, _fx) = two_fetches_against_rejected_credential(
        "dead_reused",
        StatusCode::UNAUTHORIZED,
        "refresh_token_reused",
    )
    .await;

    assert_eq!(
        server.token_calls().len(),
        1,
        "the second invocation must reuse the recorded verdict, saw {:?}",
        server.token_calls()
    );
    assert_eq!(
        server.usage_calls().len(),
        1,
        "a credential the server already rejected must not reach the usage API again, saw {:?}",
        server.usage_calls()
    );
    assert_eq!(
        second.summary, first.summary,
        "the cached verdict must render exactly like the live one"
    );
    assert!(
        second.summary.contains("refresh_token_reused"),
        "the cached verdict must still name the server's reason: {}",
        second.summary
    );
    server.shutdown();
}

/// The other verdict seen in production. It is equally terminal, so it has to
/// be recognised by code rather than inferred from the 4xx status alone —
/// status-only classification cannot distinguish it from a proxy's 403.
#[tokio::test]
async fn a_session_ended_verdict_is_not_re_presented_on_the_next_invocation() {
    let _lock = ENV_LOCK.lock().await;
    let (server, _first, second, _fx) = two_fetches_against_rejected_credential(
        "dead_invalidated",
        StatusCode::UNAUTHORIZED,
        "refresh_token_invalidated",
    )
    .await;

    assert_eq!(
        server.token_calls().len(),
        1,
        "`refresh_token_invalidated` is as final as a reuse verdict, saw {:?}",
        server.token_calls()
    );
    assert!(
        second.summary.contains("refresh_token_invalidated"),
        "the cached verdict must still name the server's reason: {}",
        second.summary
    );
    server.shutdown();
}

/// Guard on the opposite side: only a verdict the auth server *named* may be
/// remembered. A bare 4xx can come from a corporate proxy, a WAF, or a captive
/// portal sitting in front of the real endpoint; remembering one of those would
/// mark a perfectly good account dead until the user found `--force`.
#[tokio::test]
async fn an_unnamed_client_error_is_still_re_presented_on_the_next_invocation() {
    let _lock = ENV_LOCK.lock().await;
    let (server, _first, _second, _fx) = two_fetches_against_rejected_credential(
        "maybe_proxy",
        StatusCode::FORBIDDEN,
        "some_gateway_verdict",
    )
    .await;

    assert_eq!(
        server.token_calls().len(),
        2,
        "an unrecognised 4xx is not proof the credential is dead; it must be retried, saw {:?}",
        server.token_calls()
    );
    server.shutdown();
}

/// The recorded verdict belongs to a *credential*, not to an alias. Signing in
/// again replaces the refresh token, and that alone has to clear the record —
/// binding it to the alias would need every write path (login, import, live
/// re-sync, daemon) to remember to clear it, and the one that forgets leaves
/// the user staring at "re-login required" after having just logged in.
#[tokio::test]
async fn signing_in_again_clears_the_recorded_verdict() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let fresh_access = jwt_expiring_in(3_600);
    let server = MockServer::start(
        vec![
            (
                stale_access.clone(),
                vec![reply(
                    StatusCode::UNAUTHORIZED,
                    json!({"detail": "expired"}),
                )],
            ),
            (fresh_access.clone(), vec![usage_ok()]),
        ],
        vec![reused_refresh_reply()],
    )
    .await;
    let fx = fixture(&server, "revived", &stale_access);

    codex_switch::usage::fetch_usage_retried("revived", &fx.profile_path, "revived")
        .await
        .expect_err("the first credential is spent");

    // What `login` / `import` leave behind: a different refresh token.
    write_auth_file(&fx.profile_path, "new_id", &fresh_access, "refresh_new");

    let usage = codex_switch::usage::fetch_usage_retried("revived", &fx.profile_path, "revived")
        .await
        .expect("a freshly signed-in profile must be fetched, not written off");

    assert!(
        usage.primary.is_some(),
        "the revived profile must report real usage"
    );
    server.shutdown();
}

/// The tail the user actually sees: `list` prints its final screen and then
/// blocks, because opportunistic refresh picks candidates purely by expiry and
/// re-presents the very tokens the fetch above just had rejected.
#[tokio::test]
async fn opportunistic_refresh_skips_a_credential_the_server_already_rejected() {
    let _lock = ENV_LOCK.lock().await;
    let stale_access = expired_jwt();
    let server = MockServer::start_with(
        vec![(
            stale_access.clone(),
            vec![reply(
                StatusCode::UNAUTHORIZED,
                json!({"detail": "expired"}),
            )],
        )],
        Vec::new(),
        HashMap::from([("refresh_old".to_string(), reused_refresh_reply())]),
    )
    .await;
    let fx = fixture(&server, "dead_tail", &stale_access);

    codex_switch::usage::fetch_usage_retried("dead_tail", &fx.profile_path, "dead_tail")
        .await
        .expect_err("the credential is spent");
    let after_fetch = server.token_calls().len();

    let failures = codex_switch::usage::refresh_expiring_tokens().await;

    assert!(
        failures.is_empty(),
        "skipping a dead credential is not a persist failure: {failures:?}"
    );
    assert_eq!(
        server.token_calls().len(),
        after_fetch,
        "a credential the server already rejected must not be refreshed in the \
         background either, saw {:?}",
        server.token_calls()
    );
    server.shutdown();
}

#[tokio::test]
async fn opportunistic_refresh_remembers_a_terminal_verdict_it_discovers() {
    let _lock = ENV_LOCK.lock().await;
    let server = MockServer::start_keyed_by_refresh_token(vec![(
        "refresh_dead_tail".to_string(),
        reused_refresh_reply(),
    )])
    .await;
    let _fx = expiring_profiles_fixture(&server, &["dead_tail"]);

    let first_failures = codex_switch::usage::refresh_expiring_tokens().await;
    assert!(
        first_failures.is_empty(),
        "a rejected credential is not a persistence failure: {first_failures:?}"
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_dead_tail".to_string()],
        "the first opportunistic pass must contact the auth server once"
    );

    let second_failures = codex_switch::usage::refresh_expiring_tokens().await;
    assert!(
        second_failures.is_empty(),
        "skipping a known rejection is not a persistence failure: {second_failures:?}"
    );
    assert_eq!(
        server.token_calls(),
        vec!["refresh_dead_tail".to_string()],
        "a terminal verdict discovered by opportunistic refresh must prevent the next replay"
    );
    server.shutdown();
}

#[tokio::test]
async fn successful_usage_after_proactive_rejection_still_records_terminal_verdict() {
    let _lock = ENV_LOCK.lock().await;
    let expiring_access = jwt_expiring_in(30);
    let server = MockServer::start(
        vec![(expiring_access.clone(), vec![usage_ok()])],
        vec![reused_refresh_reply()],
    )
    .await;
    let fx = fixture(&server, "proactive_rejection", &expiring_access);

    let usage = codex_switch::usage::fetch_usage_retried(
        "proactive_rejection",
        &fx.profile_path,
        "proactive_rejection",
    )
    .await
    .expect("the still-valid access token should return current usage");
    assert!(usage.primary.is_some());
    assert_eq!(server.token_calls(), vec!["refresh_old".to_string()]);

    let failures = codex_switch::usage::refresh_expiring_tokens().await;

    assert!(failures.is_empty());
    assert_eq!(
        server.token_calls(),
        vec!["refresh_old".to_string()],
        "the background refresher must not replay a credential already rejected during a successful usage fetch"
    );
    server.shutdown();
}

// ── who is allowed to ask again ──────────────────────────────────────────
//
// "Ignore the cache" turned out to be two different requests wearing one
// boolean: wanting numbers that are not stale, and wanting a verdict the auth
// server already gave to be re-litigated. Only a person can mean the second.

/// Set up a profile whose credential the server has already rejected, and
/// return the mock plus the call count at that point.
async fn profile_with_a_recorded_verdict(alias: &'static str) -> (MockServer, Fixture, usize) {
    let stale_access = expired_jwt();
    let server = MockServer::start(
        vec![(
            stale_access.clone(),
            vec![reply(
                StatusCode::UNAUTHORIZED,
                json!({"detail": "expired"}),
            )],
        )],
        vec![reused_refresh_reply()],
    )
    .await;
    let fx = fixture(&server, alias, &stale_access);

    codex_switch::usage::fetch_usage_retried(alias, &fx.profile_path, alias)
        .await
        .expect_err("the credential is spent");
    let calls = server.token_calls().len();

    (server, fx, calls)
}

/// The daemon polls on a timer and wants numbers that are not stale. It cannot
/// want a spent credential re-presented: the answer is known, nobody is
/// watching, and at a few seconds per rejection this runs every polling
/// interval for as long as the daemon is up.
#[tokio::test]
async fn an_unattended_refresh_does_not_re_present_a_rejected_credential() {
    let _lock = ENV_LOCK.lock().await;
    let (server, fx, after_first) = profile_with_a_recorded_verdict("daemon_dead").await;

    let err = codex_switch::usage::fetch_usage_retried_unattended(
        "daemon_dead",
        &fx.profile_path,
        "daemon_dead",
    )
    .await
    .expect_err("the account is still unusable");

    assert_eq!(
        server.token_calls().len(),
        after_first,
        "an unattended refresh must honour a verdict already on record, saw {:?}",
        server.token_calls()
    );
    assert!(
        err.summary.contains("re-login required"),
        "it must still report why the account is unusable: {}",
        err.summary
    );
    server.shutdown();
}

/// The other side of that split: `--force` is a person saying "ask anyway", and
/// it has to keep reaching the server or there is no way back from a verdict
/// recorded in error.
#[tokio::test]
async fn an_explicit_force_still_re_presents_a_rejected_credential() {
    let _lock = ENV_LOCK.lock().await;
    let (server, fx, after_first) = profile_with_a_recorded_verdict("forced_dead").await;

    codex_switch::usage::fetch_usage_retried_force("forced_dead", &fx.profile_path, "forced_dead")
        .await
        .expect_err("the credential really is spent");

    assert_eq!(
        server.token_calls().len(),
        after_first + 1,
        "force is the escape hatch and must reach the server, saw {:?}",
        server.token_calls()
    );
    server.shutdown();
}

/// The other half of the same split, and the half with no user watching it:
/// an unattended refresh still has to ignore the usage TTL. The daemon decides
/// whether to switch accounts on these numbers, so serving it a cached figure
/// would have it act on quota that may be hours old.
#[tokio::test]
async fn an_unattended_refresh_still_ignores_a_fresh_usage_cache() {
    let _lock = ENV_LOCK.lock().await;
    let access = jwt_expiring_in(7_200);
    let server =
        MockServer::start(vec![(access.clone(), vec![usage_ok()])], vec![rotation(1)]).await;
    let fx = fixture(&server, "warm", &access);

    codex_switch::usage::fetch_usage_retried("warm", &fx.profile_path, "warm")
        .await
        .expect("the first read populates the cache");
    assert_eq!(
        server.usage_calls().len(),
        1,
        "the first read must reach the usage API, saw {:?}",
        server.usage_calls()
    );

    codex_switch::usage::fetch_usage_retried("warm", &fx.profile_path, "warm")
        .await
        .expect("a cached read succeeds");
    assert_eq!(
        server.usage_calls().len(),
        1,
        "an ordinary read must be served from cache, saw {:?}",
        server.usage_calls()
    );

    codex_switch::usage::fetch_usage_retried_unattended("warm", &fx.profile_path, "warm")
        .await
        .expect("an unattended refresh succeeds");
    assert_eq!(
        server.usage_calls().len(),
        2,
        "an unattended refresh must fetch current numbers, saw {:?}",
        server.usage_calls()
    );
    server.shutdown();
}

#[tokio::test]
async fn usage_429_returns_immediately_and_cools_down_the_account() {
    let _lock = ENV_LOCK.lock().await;
    let access = jwt_expiring_in(7_200);
    let server = MockServer::start(
        vec![(
            access.clone(),
            vec![
                reply(
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({"detail": "slow down"}),
                ),
                usage_ok(),
                usage_ok(),
            ],
        )],
        vec![rotation(1)],
    )
    .await;
    let fx = fixture(&server, "usage-cooldown", &access);

    let first = tokio::time::timeout(
        Duration::from_secs(2),
        codex_switch::usage::fetch_usage_retried_unattended(
            "usage-cooldown",
            &fx.profile_path,
            "usage-cooldown",
        ),
    )
    .await
    .expect("HTTP 429 must be returned to the scheduler instead of sleeping")
    .expect_err("the first response is rate limited");
    assert!(first.summary.contains("429"), "{}", first.summary);
    let calls_after_first = server.usage_calls().len();

    codex_switch::usage::fetch_usage_retried_unattended(
        "usage-cooldown",
        &fx.profile_path,
        "usage-cooldown",
    )
    .await
    .expect_err("the account remains in its shared cooldown");
    assert_eq!(
        server.usage_calls().len(),
        calls_after_first,
        "a later daemon pass must not contact an account still cooling down"
    );

    codex_switch::usage::fetch_usage_retried_force(
        "usage-cooldown",
        &fx.profile_path,
        "usage-cooldown",
    )
    .await
    .expect("an explicit refresh bypasses the cooldown");
    let calls_after_force = server.usage_calls().len();
    codex_switch::usage::fetch_usage_retried_unattended(
        "usage-cooldown",
        &fx.profile_path,
        "usage-cooldown",
    )
    .await
    .expect("a successful force refresh clears the shared cooldown");
    assert_eq!(server.usage_calls().len(), calls_after_force + 1);
    server.shutdown();
}

/// `invalid_grant` is standard OAuth wording, emitted by assorted servers and
/// intermediaries for conditions that are not "this token is spent" — clock
/// skew among them. Stopping the retry loop on it is right; remembering it
/// until the next sign-in is not, because a transient cause would strand a
/// working account behind a message telling the user to log in again.
#[tokio::test]
async fn a_generic_oauth_rejection_is_not_remembered_across_invocations() {
    let _lock = ENV_LOCK.lock().await;
    let (server, first, _second, _fx) = two_fetches_against_rejected_credential(
        "generic_reject",
        StatusCode::UNAUTHORIZED,
        "invalid_grant",
    )
    .await;

    assert_eq!(
        server.token_calls().len(),
        2,
        "only verdicts naming a spent credential may outlive the invocation, saw {:?}",
        server.token_calls()
    );
    assert!(
        first.summary.contains("re-login required"),
        "it must still stop the retry loop and say so within the call: {}",
        first.summary
    );
    server.shutdown();
}
