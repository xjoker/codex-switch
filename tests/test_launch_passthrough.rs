//! End-to-end argv contract for `codex-switch launch -- …`.
//!
//! A fake `codex` on PATH records the exact argument vector it received, so
//! these tests prove the composed command rather than only the clap parse.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::oneshot;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-switch-launch-{name}-{ts}-{id}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn jwt(payload: &Value) -> String {
    let json = serde_json::to_vec(payload).unwrap();
    let encoded = {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD.encode(json)
    };
    format!("x.{encoded}.y")
}

fn write_auth(path: &Path, email: &str, account_id: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let claims = serde_json::json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus",
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
            "organizations": [],
        }
    });
    let auth = serde_json::json!({
        "tokens": {
            "id_token": jwt(&claims),
            "refresh_token": "dummy-refresh",
            "access_token": "dummy-access",
            "account_id": account_id,
        },
        "last_refresh": "2026-08-01T00:00:00Z",
    });
    fs::write(path, serde_json::to_string_pretty(&auth).unwrap()).unwrap();
}

const FAKE_CODEX_PY: &str = r#"import json, os, sys
path = os.environ["CS_FAKE_CODEX_LOG"]
try:
    data = json.loads(open(path, encoding="utf-8").read())
except Exception:
    data = []
data.append({
    "argv": sys.argv[1:],
    "pid": os.getpid(),
    "codex_home": os.environ.get("CODEX_HOME"),
})
open(path, "w", encoding="utf-8").write(json.dumps(data))

# A real Codex invocation creates its session index and rollout under the
# CODEX_HOME it received.  The fixture is opt-in so the older argv-only tests
# keep exercising the same small fake.
session_id = os.environ.get("CS_FAKE_CODEX_SESSION_ID")
if session_id and sys.argv[1:] != ["--version"]:
    codex_home = os.environ["CODEX_HOME"]
    session_name = os.environ.get("CS_FAKE_CODEX_SESSION_NAME", session_id)
    provider = os.environ.get("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter")
    model = os.environ.get("CS_FAKE_CODEX_SESSION_MODEL", "openai/gpt-5.3-codex")
    updated_at = os.environ.get("CS_FAKE_CODEX_SESSION_UPDATED_AT", "2026-09-09T00:00:00Z")
    day = os.path.join(codex_home, "sessions", "2026", "09", "09")
    os.makedirs(day, exist_ok=True)
    rollout = os.path.join(day, "rollout-" + session_id + ".jsonl")
    meta = {
        "type": "session_meta",
        "id": session_id,
        "name": session_name,
        "model_provider": provider,
        "model": model,
        "updated_at": updated_at,
    }
    with open(rollout, "w", encoding="utf-8") as stream:
        stream.write(json.dumps(meta) + "\n")
    index = os.path.join(codex_home, "session_index.jsonl")
    with open(index, "a", encoding="utf-8") as stream:
        stream.write(json.dumps({
            "id": session_id,
            "name": session_name,
            "thread_name": session_name,
            "model_provider": provider,
            "model": model,
            "updated_at": updated_at,
            "rollout_path": os.path.relpath(rollout, codex_home).replace(os.sep, "/"),
        }) + "\n")
if sys.argv[1:] == ["--version"]:
    sys.stdout.write("codex-cli 0.0.0-test\n")
else:
    delay = float(os.environ.get("CS_FAKE_CODEX_SLEEP", "0"))
    if delay:
        import time
        time.sleep(delay)
    size = int(os.environ.get("CS_FAKE_CODEX_STDOUT_BYTES", "0"))
    sys.stdout.write("x" * size if size else "codex-ok\n")
sys.exit(0)
"#;

const ARGV_EDGE_CASE: &str = "review with spaces 世界 & echo should-not-run";

#[cfg(windows)]
fn locate_python() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for name in ["python.exe", "python3.exe", "py.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    panic!("Windows launch tests require an existing Python executable on PATH");
}

fn install_fake_codex(home: &Path) -> (PathBuf, PathBuf) {
    let bin_dir = home.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let log = home.join("fake-codex-log.json");
    fs::write(&log, "[]").unwrap();
    #[cfg(unix)]
    {
        let script = bin_dir.join("codex");
        fs::write(&script, format!("#!/usr/bin/env python3\n{FAKE_CODEX_PY}")).unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();
    }
    #[cfg(windows)]
    {
        let script = bin_dir.join("fake_codex.py");
        fs::write(&script, FAKE_CODEX_PY).unwrap();
        let python = locate_python();
        let python = python.to_string_lossy().replace('"', "\"\"");
        fs::write(
            bin_dir.join("codex.cmd"),
            format!("@echo off\r\n\"{python}\" \"%~dp0fake_codex.py\" %*\r\n"),
        )
        .unwrap();
    }
    (bin_dir, log)
}

fn recorded_argv(log: &Path) -> Vec<Vec<String>> {
    let raw = fs::read_to_string(log).unwrap();
    let data: Vec<Value> = serde_json::from_str(&raw).unwrap();
    data.into_iter()
        .map(|entry| {
            entry["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

#[derive(Debug, Clone)]
struct FakeLaunch {
    argv: Vec<String>,
    codex_home: PathBuf,
}

fn recorded_launches(log: &Path) -> Vec<FakeLaunch> {
    let raw = fs::read_to_string(log).unwrap();
    let data: Vec<Value> = serde_json::from_str(&raw).unwrap();
    data.into_iter()
        .filter_map(|entry| {
            let argv: Vec<String> = entry["argv"]
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            if argv.as_slice() == ["--version"] {
                return None;
            }
            Some(FakeLaunch {
                argv,
                codex_home: PathBuf::from(entry["codex_home"].as_str().unwrap()),
            })
        })
        .collect()
}

fn last_non_version_argv(log: &Path) -> Vec<String> {
    recorded_argv(log)
        .into_iter()
        .rev()
        .find(|argv| argv.as_slice() != ["--version"])
        .expect("fake codex must have been launched with real args")
}

#[cfg(unix)]
fn last_non_version_pid(log: &Path) -> Option<u32> {
    let raw = fs::read_to_string(log).ok()?;
    let data: Vec<Value> = serde_json::from_str(&raw).ok()?;
    data.into_iter()
        .rev()
        .find(|entry| {
            entry["argv"]
                .as_array()
                .is_some_and(|argv| argv.iter().any(|arg| arg.as_str() != Some("--version")))
        })
        .and_then(|entry| entry["pid"].as_u64())
        .and_then(|pid| u32::try_from(pid).ok())
}

fn command(home: &Path, fake_bin: &Path, log: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-switch"));
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env("HOME", home);
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.env("CODEX_SWITCH_HOME", home.join(".codex-switch"));
    cmd.env("CS_FAKE_CODEX_LOG", log);
    #[cfg(unix)]
    cmd.env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()));
    #[cfg(windows)]
    {
        let mut paths = vec![fake_bin.to_path_buf()];
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let system_root = PathBuf::from(system_root);
            paths.push(system_root.join("System32"));
            paths.push(system_root);
        }
        cmd.env("PATH", std::env::join_paths(paths).unwrap());
    }
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

fn run(home: &Path, fake_bin: &Path, log: &Path, args: &[&str]) -> Output {
    command(home, fake_bin, log, args).output().unwrap()
}

fn run_env(
    home: &Path,
    fake_bin: &Path,
    log: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let mut cmd = command(home, fake_bin, log, args);
    for (name, value) in env {
        cmd.env(name, value);
    }
    cmd.output().unwrap()
}

fn wait_for_launch_count(log: &Path, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while recorded_launches(log).len() < count {
        assert!(
            Instant::now() < deadline,
            "fake codex did not start {count} times"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn setup_provider_at(home: &Path, base_url: &str) {
    setup_provider_named_at(home, "openrouter", "openrouter", base_url);
}

fn setup_provider_named_at(home: &Path, alias: &str, provider_id: &str, base_url: &str) {
    let dir = home.join(format!(".codex-switch/providers/{alias}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("provider.toml"),
        format!(
            r#"provider_id = "{provider_id}"
name = "{alias}"
base_url = "{base_url}"
allow_insecure_http = true
env_key = "CODEX_SWITCH_{provider_id}_KEY"
default_model = "openai/gpt-5.3-codex"
wire_api = "responses"
api_key = "sk-test-passthrough"
metadata_fallback = "none"

[[models]]
id = "openai/gpt-5.3-codex"

[[models]]
id = "deepseek/deepseek-r1-0528"
reasoning = "high"
no_web_search = true
"#
        ),
    )
    .unwrap();
}

fn write_session_fixture(
    codex_home: &Path,
    session_id: &str,
    session_name: &str,
    provider: &str,
    model: &str,
    updated_at: &str,
) {
    let day = codex_home.join("sessions/2026/09/09");
    fs::create_dir_all(&day).unwrap();
    let rollout = day.join(format!("rollout-{session_id}.jsonl"));
    let meta = serde_json::json!({
        "type": "session_meta",
        "id": session_id,
        "name": session_name,
        "model_provider": provider,
        "model": model,
        "updated_at": updated_at,
    });
    fs::write(
        &rollout,
        format!("{}\n", serde_json::to_string(&meta).unwrap()),
    )
    .unwrap();
    let index = codex_home.join("session_index.jsonl");
    let mut stream = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index)
        .unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "id": session_id,
            "name": session_name,
            "thread_name": session_name,
            "model_provider": provider,
            "model": model,
            "updated_at": updated_at,
            "rollout_path": format!("sessions/2026/09/09/rollout-{session_id}.jsonl"),
        })
    )
    .unwrap();
}

fn setup_provider(home: &Path) {
    setup_provider_at(home, "http://127.0.0.1:9/v1");
}

async fn provider_models_handler(State(count): State<Arc<AtomicUsize>>) -> Json<Value> {
    count.fetch_add(1, Ordering::Relaxed);
    Json(serde_json::json!({
        "data": [{"id": "openai/gpt-5.3-codex", "context_length": 123456}],
    }))
}

async fn provider_responses_handler(
    State(count): State<Arc<AtomicUsize>>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    count.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": {"type": "invalid_request_error", "message": "input required"},
        })),
    )
}

async fn provider_unexpected_handler(
    State(count): State<Arc<AtomicUsize>>,
) -> (StatusCode, Json<Value>) {
    count.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "unexpected provider request"})),
    )
}

struct RequestCounter {
    base_url: String,
    count: Arc<AtomicUsize>,
    _shutdown: oneshot::Sender<()>,
    _rt: Runtime,
}

impl RequestCounter {
    fn start() -> Self {
        let count = Arc::new(AtomicUsize::new(0));
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/models", get(provider_models_handler))
            .route("/v1/responses", post(provider_responses_handler))
            .fallback(provider_unexpected_handler)
            .with_state(count.clone());
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        rt.spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            count,
            _shutdown: shutdown,
            _rt: rt,
        }
    }

    fn requests(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

fn setup_chatgpt(home: &Path) {
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .unwrap();
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(
        home.join(".codex-switch/config.toml"),
        "[launch]\nrestore_delay_secs = 1\n",
    )
    .unwrap();
    write_auth(
        &home.join(".codex-switch/profiles/work/auth.json"),
        "work@example.com",
        "acct_work",
    );
    fs::write(home.join(".codex-switch/current"), "work").unwrap();
}

#[test]
fn launch_dash_dash_exec_json_is_not_an_alias_named_exec() {
    let home = temp_home("dash-dash-exec");
    let (fake_bin, log) = install_fake_codex(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "--", "exec", "--json", "review this"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "auto-select with no profiles must fail: {combined}"
    );
    assert!(
        combined.contains("no saved profiles"),
        "launch -- exec must auto-select, not look up alias exec: {combined}"
    );
    assert!(
        !combined.contains("Profile 'exec' not found"),
        "launch -- exec must not treat exec as an alias: {combined}"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_puts_cs_model_after_exec() {
    let home = temp_home("chatgpt-model-exec");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &[
            "launch",
            "work",
            "--model",
            "gpt-5.4",
            "--",
            "exec",
            "--json",
            ARGV_EDGE_CASE,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["exec", "--model", "gpt-5.4", "--json", ARGV_EDGE_CASE]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_provider_puts_c_overrides_after_exec_and_keeps_json() {
    let home = temp_home("provider-exec");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &[
            "launch",
            "openrouter",
            "--",
            "exec",
            "--json",
            "--color",
            "never",
            ARGV_EDGE_CASE,
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv = last_non_version_argv(&log);
    assert_eq!(argv.first().map(String::as_str), Some("exec"));
    let exec_at = 0;
    assert!(
        argv[exec_at + 1..]
            .windows(2)
            .any(|pair| pair[0] == "-c" && pair[1].starts_with("model_provider=")),
        "-c model_provider must follow exec: {argv:?}"
    );
    assert!(
        argv[exec_at + 1..]
            .windows(2)
            .any(|pair| pair[0] == "-c" && pair[1].starts_with("model=")),
        "-c model must follow exec when the user did not pass --model: {argv:?}"
    );
    let json_at = argv.iter().position(|a| a == "--json").expect("--json");
    assert_eq!(
        &argv[json_at..],
        ["--json", "--color", "never", ARGV_EDGE_CASE]
    );
    assert!(
        !argv.iter().any(|a| a.contains("sk-test-passthrough")),
        "API key must not appear in argv: {argv:?}"
    );
    let _ = fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn cli_provider_sigterm_reaps_codex_and_exits_143() {
    use std::os::unix::process::ExitStatusExt;

    let home = temp_home("provider-sigterm");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let mut child = command(&home, &fake_bin, &log, &["launch", "openrouter"])
        .env("CS_FAKE_CODEX_SLEEP", "30")
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let codex_pid = loop {
        if let Some(pid) = last_non_version_pid(&log) {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "Codex did not start");
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143), "status={status:?}");
    assert_eq!(status.signal(), None);
    assert_eq!(
        unsafe { libc::kill(codex_pid as i32, 0) },
        -1,
        "provider Codex child must be reaped"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_provider_passthrough_model_drops_saved_model_overrides() {
    let home = temp_home("provider-oneshot-model");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "--", "-m", "one-shot", "exec", "hi"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv = last_non_version_argv(&log);
    assert!(
        !argv.windows(2).any(|pair| pair[0] == "-c"
            && (pair[1].starts_with("model=")
                || pair[1].starts_with("model_reasoning_effort=")
                || pair[1].starts_with("web_search="))),
        "passthrough -m must drop per-model -c pairs: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "-c" && pair[1].starts_with("model_provider=")),
        "provider definition must remain: {argv:?}"
    );
    assert_eq!(argv.first().map(String::as_str), Some("exec"));
    let model_at = argv.iter().position(|a| a == "-m").expect("-m");
    assert!(model_at > 0, "-m must follow exec: {argv:?}");
    assert_eq!(&argv[model_at..], ["-m", "one-shot", "hi"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_exec_without_separator_is_not_an_alias() {
    let home = temp_home("exec-not-alias");
    let (fake_bin, log) = install_fake_codex(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "exec", "--json", "review this"],
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "auto-select with no profiles must fail: {combined}"
    );
    assert!(
        combined.contains("no saved profiles"),
        "launch exec must auto-select, not look up alias exec: {combined}"
    );
    assert!(!combined.contains("Profile 'exec' not found"), "{combined}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_merges_tokens_on_both_sides_of_double_dash() {
    let home = temp_home("merge-dash");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "exec", "--", "--json", "hi"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(last_non_version_argv(&log), ["exec", "--json", "hi"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_json_reports_passthrough_model_and_captures_codex_stdout() {
    const LIMIT: usize = 1024 * 1024;
    let home = temp_home("json-model");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);

    let output = command(
        &home,
        &fake_bin,
        &log,
        &[
            "--json",
            "launch",
            "openrouter",
            "--",
            "-m",
            "one-shot",
            "exec",
            "hi",
        ],
    )
    .env("CS_FAKE_CODEX_STDOUT_BYTES", (LIMIT + 4096).to_string())
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["alias"], "openrouter");
    assert_eq!(payload["model"], "one-shot");
    assert!(payload["codex_stdout"].as_str().unwrap().len() <= LIMIT);
    assert_eq!(payload["codex_stdout_truncated"], true);
    assert_eq!(payload["codex_stderr_truncated"], false);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_sync_is_persisted_and_launch_stays_offline() {
    let home = temp_home("provider-probe-verdict");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);

    let fetched = run(
        &home,
        &fake_bin,
        &log,
        &["provider", "fetch-models", "openrouter"],
    );
    assert!(
        fetched.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&fetched.stderr)
    );

    let probed = run(
        &home,
        &fake_bin,
        &log,
        &[
            "provider",
            "probe",
            "openrouter",
            "--model",
            "openai/gpt-5.3-codex",
        ],
    );

    assert!(
        probed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&probed.stderr)
    );
    let saved =
        fs::read_to_string(home.join(".codex-switch/providers/openrouter/provider.toml")).unwrap();
    assert!(
        saved.contains("responses_support") && saved.contains("openai/gpt-5.3-codex"),
        "an explicit probe must make its verdict available to later offline launches: {saved}"
    );
    let catalog = home.join(".codex-switch/providers/openrouter/models.json");
    assert!(
        catalog.exists(),
        "explicit model sync must persist launch metadata"
    );
    let saved_catalog = fs::read_to_string(&catalog).unwrap();
    let catalog_json: Value = serde_json::from_str(&saved_catalog).unwrap();
    assert_eq!(catalog_json["models"][0]["slug"], "openai/gpt-5.3-codex");
    assert_eq!(catalog_json["models"][0]["context_window"], 123456);

    let requests_before_launch = server.requests();
    let launched = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    assert!(
        launched.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&launched.stderr)
    );
    assert_eq!(
        server.requests(),
        requests_before_launch,
        "launch must consume the saved probe/catalog without network I/O"
    );
    assert_eq!(
        fs::read_to_string(catalog).unwrap(),
        saved_catalog,
        "launch must not replace fetched metadata with local defaults"
    );
    let path = home.join(".codex-switch/providers/openrouter/provider.toml");
    let unsupported = fs::read_to_string(&path).unwrap().replace(
        "\"openai/gpt-5.3-codex\" = true",
        "\"openai/gpt-5.3-codex\" = false",
    );
    assert!(unsupported.contains("\"openai/gpt-5.3-codex\" = false"));
    fs::write(path, unsupported).unwrap();
    let codex_launches = recorded_argv(&log).len();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "saved unsupported verdict was ignored"
    );
    assert!(
        combined.contains("no Codex Responses channel"),
        "{combined}"
    );
    assert_eq!(
        server.requests(),
        requests_before_launch,
        "a saved unsupported verdict must also be enforced offline"
    );
    assert_eq!(recorded_argv(&log).len(), codex_launches);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_launches_use_distinct_homes_per_run_and_provider() {
    let home = temp_home("provider-resume-一致性");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    setup_provider_named_at(&home, "second", "second", "http://127.0.0.1:9/v1");

    for (alias, id, provider) in [
        ("openrouter", "first-run", "openrouter"),
        ("openrouter", "second-run", "openrouter"),
        ("second", "other-run", "second"),
    ] {
        let output = run_env(
            &home,
            &fake_bin,
            &log,
            &["launch", alias],
            &[
                ("CS_FAKE_CODEX_SESSION_ID", id),
                ("CS_FAKE_CODEX_SESSION_PROVIDER", provider),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let launches = recorded_launches(&log);
    assert_eq!(launches.len(), 3);
    assert_ne!(launches[0].codex_home, launches[1].codex_home);
    assert_ne!(launches[0].codex_home, launches[2].codex_home);
    assert_ne!(launches[1].codex_home, launches[2].codex_home);
    assert!(launches[0].codex_home.to_string_lossy().contains("一致性"));
    assert_eq!(
        launches[0].codex_home.parent(),
        launches[1].codex_home.parent()
    );
    assert_ne!(
        launches[0].codex_home.parent(),
        launches[2].codex_home.parent()
    );
    assert_eq!(
        launches[0]
            .codex_home
            .parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str()),
        Some("provider-runs")
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_uses_named_session_and_latest_run_of_that_provider() {
    let home = temp_home("provider-resume-index-一致性");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    setup_provider_named_at(&home, "second", "second", "http://127.0.0.1:9/v1");
    write_session_fixture(
        &home.join(".codex"),
        "global-session",
        "global session",
        "openrouter",
        "openai/gpt-5.3-codex",
        "2026-09-09T23:59:59Z",
    );

    let first = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "session-唯一"),
            ("CS_FAKE_CODEX_SESSION_NAME", "唯一会话"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter"),
            ("CS_FAKE_CODEX_SESSION_UPDATED_AT", "2026-09-09T00:00:01Z"),
        ],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_home = recorded_launches(&log)[0].codex_home.clone();

    let other = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "second"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "other-session"),
            ("CS_FAKE_CODEX_SESSION_NAME", "other session"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "second"),
            ("CS_FAKE_CODEX_SESSION_UPDATED_AT", "2026-09-09T00:00:03Z"),
        ],
    );
    assert!(
        other.status.success(),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    let second_home = recorded_launches(&log)[1].codex_home.clone();

    let latest = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "session-latest"),
            ("CS_FAKE_CODEX_SESSION_NAME", "最近会话"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter"),
            ("CS_FAKE_CODEX_SESSION_UPDATED_AT", "2026-09-09T00:00:02Z"),
        ],
    );
    assert!(
        latest.status.success(),
        "{}",
        String::from_utf8_lossy(&latest.stderr)
    );
    let latest_home = recorded_launches(&log)[2].codex_home.clone();

    let named = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "唯一会话"],
    );
    assert!(
        named.status.success(),
        "{}",
        String::from_utf8_lossy(&named.stderr)
    );
    let named_run = &recorded_launches(&log)[3];
    assert_eq!(named_run.codex_home, first_home);
    assert_eq!(named_run.argv.first().map(String::as_str), Some("resume"));
    assert_eq!(
        named_run.argv.last().map(String::as_str),
        Some("session-唯一")
    );

    let last = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "--last"],
    );
    assert!(
        last.status.success(),
        "{}",
        String::from_utf8_lossy(&last.stderr)
    );
    let last_run = &recorded_launches(&log)[4];
    assert_eq!(last_run.codex_home, latest_home);
    assert_ne!(last_run.codex_home, second_home);
    assert!(!last_run.argv.contains(&"--last".to_string()));
    assert_eq!(
        last_run.argv.last().map(String::as_str),
        Some("session-latest")
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_skips_value_options_and_preserves_the_prompt() {
    let home = temp_home("provider-resume-args");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let created = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "resume-args-session"),
            ("CS_FAKE_CODEX_SESSION_NAME", "resume args"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter"),
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let resumed = run(
        &home,
        &fake_bin,
        &log,
        &[
            "launch",
            "openrouter",
            "resume",
            "-C",
            "D:\\workspace\\project",
            "--sandbox",
            "workspace-write",
            "-i",
            "image.png",
            "--remote",
            "remote-name",
            "resume-args-session",
            "continue",
            "this task",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let argv = &recorded_launches(&log)[1].argv;
    assert_eq!(argv.first().map(String::as_str), Some("resume"));
    assert!(
        argv.windows(2)
            .any(|pair| { pair == ["-C", "D:\\workspace\\project"] })
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--sandbox", "workspace-write"])
    );
    assert!(argv.windows(2).any(|pair| pair == ["-i", "image.png"]));
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--remote", "remote-name"])
    );
    let session = argv
        .iter()
        .position(|arg| arg == "resume-args-session")
        .expect("exact session id must be forwarded");
    assert_eq!(
        &argv[session..],
        ["resume-args-session", "continue", "this task"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_rejects_ambiguous_or_cross_provider_session_before_codex() {
    let home = temp_home("provider-resume-errors");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    setup_provider_named_at(&home, "second", "second", "http://127.0.0.1:9/v1");

    for (alias, id, name) in [
        ("openrouter", "ambiguous-a", "same name"),
        ("openrouter", "ambiguous-b", "same name"),
        ("second", "foreign-id", "foreign session"),
    ] {
        let output = run_env(
            &home,
            &fake_bin,
            &log,
            &["launch", alias],
            &[
                ("CS_FAKE_CODEX_SESSION_ID", id),
                ("CS_FAKE_CODEX_SESSION_NAME", name),
                ("CS_FAKE_CODEX_SESSION_PROVIDER", alias),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let before = recorded_launches(&log).len();

    let ambiguous = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "same name"],
    );
    assert!(!ambiguous.status.success());
    assert_eq!(
        recorded_launches(&log).len(),
        before,
        "ambiguous name must not start Codex"
    );

    let foreign = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "foreign-id"],
    );
    assert!(!foreign.status.success());
    assert_eq!(
        recorded_launches(&log).len(),
        before,
        "foreign provider id must not start Codex"
    );

    let bare = run(&home, &fake_bin, &log, &["launch", "resume", "--last"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    )
    .to_ascii_lowercase();
    assert!(!bare.status.success());
    assert_eq!(recorded_launches(&log).len(), before);
    assert!(
        combined.contains("provider") && combined.contains("alias"),
        "{combined}"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_history_follows_rename_but_not_remove_and_recreate() {
    let home = temp_home("provider-resume-lifecycle");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let created = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "rename-history"),
            ("CS_FAKE_CODEX_SESSION_NAME", "rename history"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter"),
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let original_home = recorded_launches(&log)[0].codex_home.clone();

    let renamed = run(
        &home,
        &fake_bin,
        &log,
        &["provider", "rename", "openrouter", "renamed"],
    );
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let resumed = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "renamed", "resume", "rename-history"],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(recorded_launches(&log)[1].codex_home, original_home);

    let removed = run(
        &home,
        &fake_bin,
        &log,
        &["provider", "remove", "renamed", "--yes"],
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let before_rebuild = recorded_launches(&log).len();
    let missing = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "renamed", "resume", "rename-history"],
    );
    assert!(!missing.status.success());
    assert_eq!(recorded_launches(&log).len(), before_rebuild);

    setup_provider_named_at(&home, "renamed", "renamed", "http://127.0.0.1:9/v1");
    let rebuilt = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "renamed", "resume", "rename-history"],
    );
    assert!(!rebuilt.status.success());
    assert_eq!(recorded_launches(&log).len(), before_rebuild);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_requires_original_model_unless_explicitly_changed() {
    let home = temp_home("provider-resume-model");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let created = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[
            ("CS_FAKE_CODEX_SESSION_ID", "model-history"),
            ("CS_FAKE_CODEX_SESSION_MODEL", "openai/gpt-5.3-codex"),
            ("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter"),
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let provider_path = home.join(".codex-switch/providers/openrouter/provider.toml");
    let mut profile = fs::read_to_string(&provider_path).unwrap();
    profile = profile.replace(
        "default_model = \"openai/gpt-5.3-codex\"",
        "default_model = \"deepseek/deepseek-r1-0528\"",
    );
    profile = profile.replace("[[models]]\nid = \"openai/gpt-5.3-codex\"\n\n", "");
    fs::write(provider_path, profile).unwrap();

    let before = recorded_launches(&log).len();
    let missing = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "model-history"],
    );
    assert!(!missing.status.success());
    assert_eq!(recorded_launches(&log).len(), before);

    let changed = run(
        &home,
        &fake_bin,
        &log,
        &[
            "launch",
            "openrouter",
            "--model",
            "deepseek/deepseek-r1-0528",
            "resume",
            "model-history",
        ],
    );
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    let argv = &recorded_launches(&log)[before].argv;
    assert_eq!(argv.first().map(String::as_str), Some("resume"));
    assert!(argv.iter().any(|arg| {
        arg == "model=deepseek/deepseek-r1-0528" || arg == "model=\"deepseek/deepseek-r1-0528\""
    }));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_serializes_same_run_but_allows_a_new_run() {
    let home = temp_home("provider-resume-lock");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let created = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[("CS_FAKE_CODEX_SESSION_ID", "locked-history")],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut running = command(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "locked-history"],
    )
    .env("CS_FAKE_CODEX_SLEEP", "2")
    .spawn()
    .unwrap();
    wait_for_launch_count(&log, 2);

    let started = Instant::now();
    let duplicate = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "locked-history"],
    );
    let elapsed = started.elapsed();
    assert!(!duplicate.status.success());
    assert!(
        elapsed < Duration::from_millis(1500),
        "same-run resume waited {elapsed:?}"
    );

    let fresh = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    let status = running.wait().unwrap();
    assert!(status.success());
    let launches = recorded_launches(&log);
    assert_ne!(launches[1].codex_home, launches.last().unwrap().codex_home);
    let _ = fs::remove_dir_all(home);
}
