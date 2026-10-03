//! End-to-end argv contract for `codex-switch launch -- …` and for the Codex
//! app-server daemon handling of `codex-switch use`.
//!
//! A fake `codex` on PATH records the exact argument vector it received, so
//! these tests prove the composed command rather than only the clap parse.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[cfg(any(unix, windows))]
use std::sync::OnceLock;
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
    "header_env": {k: v for k, v in os.environ.items() if k.startswith("CODEX_SWITCH_HEADER_")},
})
# Write to a temp file and rename it into place so a concurrent reader never
# sees a truncated, half-written log.
tmp_path = "%s.%d.tmp" % (path, os.getpid())
with open(tmp_path, "w", encoding="utf-8") as tmp:
    tmp.write(json.dumps(data))
for attempt in range(50):
    try:
        os.replace(tmp_path, path)
        break
    except PermissionError:
        # Windows refuses the rename while a reader briefly holds the target.
        import time
        time.sleep(0.02)
else:
    os.replace(tmp_path, path)

argv = sys.argv[1:]
if argv == ["--version"]:
    if os.environ.get("CS_FAKE_CODEX_VERSION_DELAY"):
        import time
        time.sleep(float(os.environ["CS_FAKE_CODEX_VERSION_DELAY"]))
    if os.environ.get("CS_FAKE_CODEX_VERSION_FAIL") == "1":
        sys.exit(7)
    sys.stdout.write("codex-cli " + os.environ.get("CS_FAKE_CODEX_VERSION", "0.159.2") + "\n")
    sys.exit(0)
if argv == ["--help"]:
    if os.environ.get("CS_FAKE_CODEX_HELP_DELAY"):
        import time
        time.sleep(float(os.environ["CS_FAKE_CODEX_HELP_DELAY"]))
    if os.environ.get("CS_FAKE_CODEX_HELP_FAIL") == "1":
        sys.exit(1)
    # Codex 0.156+ lists `--no-daemon` in its root help.
    sys.stdout.write("Usage: codex [OPTIONS] [PROMPT]\n")
    if os.environ.get("CS_FAKE_CODEX_NO_DAEMON") == "1":
        sys.stdout.write("      --no-daemon\n")
    sys.exit(0)
if argv == ["app-server", "daemon", "version"]:
    if os.environ.get("CS_FAKE_CODEX_DAEMON") == "running":
        sys.stdout.write('{"status":"running","cliVersion":"0.159.2","appServerVersion":"0.159.2"}\n')
        sys.exit(0)
    sys.stderr.write("Error: failed to connect to app-server-control.sock\n")
    sys.exit(1)
if argv == ["app-server", "daemon", "restart"]:
    if os.environ.get("CS_FAKE_CODEX_DAEMON_RESTART") == "fail":
        sys.stderr.write("Error: app server is running but is not managed by codex app-server daemon\n")
        sys.exit(1)
    sys.stdout.write('{"status":"restarted"}\n')
    sys.exit(0)

# A real Codex invocation creates its session index and rollout under the
# CODEX_HOME it received.  The fixture is opt-in so the older argv-only tests
# keep exercising the same small fake.
session_id = os.environ.get("CS_FAKE_CODEX_SESSION_ID")
if session_id:
    codex_home = os.environ["CODEX_HOME"]
    session_name = os.environ.get("CS_FAKE_CODEX_SESSION_NAME", session_id)
    provider = os.environ.get("CS_FAKE_CODEX_SESSION_PROVIDER", "openrouter")
    for arg in sys.argv[1:]:
        if arg.startswith("model_provider="):
            provider = arg.split("=", 1)[1].strip('"')
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
delay = float(os.environ.get("CS_FAKE_CODEX_SLEEP", "0"))
if delay:
    import time
    time.sleep(delay)
size = int(os.environ.get("CS_FAKE_CODEX_STDOUT_BYTES", "0"))
sys.stdout.write("x" * size if size else "codex-ok\n")
sys.stdout.flush()
if os.environ.get("CS_FAKE_CODEX_DONE"):
    open(os.environ["CS_FAKE_CODEX_DONE"], "w").write("completed")
sys.exit(0)
"#;

const ARGV_EDGE_CASE: &str = "review with spaces 世界 & echo should-not-run";

#[cfg(unix)]
fn locate_python3() -> &'static Path {
    static PYTHON3: OnceLock<PathBuf> = OnceLock::new();
    PYTHON3.get_or_init(|| {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        let python = std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("Unix launch tests require python3 on the test runner PATH"))
            .canonicalize()
            .unwrap_or_else(|error| panic!("resolving the test runner's python3: {error}"));

        // Resolve and start the test interpreter before launching codex-switch:
        // the CLI's first version probe has a strict four-second deadline.
        let output = Command::new(&python)
            .args(["-c", "pass"])
            .output()
            .unwrap_or_else(|error| panic!("starting the test runner's python3: {error}"));
        assert!(
            output.status.success(),
            "test runner python3 prewarm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        python
    })
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn locate_python() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let candidate = std::env::split_paths(&path)
            .flat_map(|dir| ["python.exe", "python3.exe", "py.exe"].map(|name| dir.join(name)))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!("Windows launch tests require an existing Python executable on PATH")
            });
        if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir()
                .unwrap_or_else(|error| {
                    panic!("resolving the test runner's current directory: {error}")
                })
                .join(candidate)
        }
    })
}

fn warm_fake_codex(home: &Path, fake_bin: &Path, log: &Path) {
    #[cfg(unix)]
    let mut command = Command::new(fake_bin.join("codex"));
    #[cfg(unix)]
    command.arg("--version");
    #[cfg(windows)]
    let mut command = Command::new(fake_bin.join("codex.cmd"));
    #[cfg(windows)]
    command.arg("--version");
    command
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("CODEX_SWITCH_HOME", home.join(".codex-switch"));
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("CS_FAKE_CODEX_")
        {
            command.env_remove(name);
        }
    }
    command
        .env("CS_FAKE_CODEX_LOG", log)
        .env("CS_FAKE_CODEX_VERSION", "0.159.2");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("warming fake Codex executable: {error}"));
    assert!(
        output.status.success(),
        "fake Codex warmup failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(log, "[]").unwrap();
}

fn install_fake_codex(home: &Path) -> (PathBuf, PathBuf) {
    let bin_dir = home.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let log = home.join("fake-codex-log.json");
    fs::write(&log, "[]").unwrap();
    #[cfg(unix)]
    {
        let python = locate_python3();
        let fake_script = bin_dir.join("fake_codex.py");
        fs::write(&fake_script, FAKE_CODEX_PY).unwrap();
        let script = bin_dir.join("codex");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec {} {} \"$@\"\n",
                shell_quote(python),
                shell_quote(&fake_script)
            ),
        )
        .unwrap();
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
    warm_fake_codex(home, &bin_dir, &log);
    (bin_dir, log)
}

/// Argv of a codex-switch probe (`--version`, `--help`, `app-server daemon …`)
/// rather than a launched Codex session.
fn is_probe(argv: &[String]) -> bool {
    matches!(argv, [flag] if flag == "--version" || flag == "--help")
        || argv.first().is_some_and(|first| first == "app-server")
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
    launches_from_entries(data)
}

/// Like [`recorded_launches`] for a log that a fake Codex may be rewriting
/// right now: an unreadable or unparsable snapshot is "not yet" rather than a
/// failure, so a poll loop retries until its own deadline.
fn try_recorded_launches(log: &Path) -> Option<Vec<FakeLaunch>> {
    let raw = fs::read_to_string(log).ok()?;
    let data: Vec<Value> = serde_json::from_str(&raw).ok()?;
    Some(launches_from_entries(data))
}

fn launches_from_entries(data: Vec<Value>) -> Vec<FakeLaunch> {
    data.into_iter()
        .filter_map(|entry| {
            let argv: Vec<String> = entry["argv"]
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            if is_probe(&argv) {
                return None;
            }
            Some(FakeLaunch {
                argv,
                codex_home: PathBuf::from(entry["codex_home"].as_str().unwrap()),
            })
        })
        .collect()
}

fn native_profile_name(launch: &FakeLaunch) -> &str {
    &launch
        .argv
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .expect("native profile")[1]
}

fn last_non_version_argv(log: &Path) -> Vec<String> {
    recorded_argv(log)
        .into_iter()
        .rev()
        .find(|argv| !is_probe(argv))
        .expect("fake codex must have been launched with real args")
}

#[cfg(unix)]
fn last_non_version_pid(log: &Path) -> Option<u32> {
    let raw = fs::read_to_string(log).ok()?;
    let data: Vec<Value> = serde_json::from_str(&raw).ok()?;
    data.into_iter()
        .rev()
        .find(|entry| {
            entry["argv"].as_array().is_some_and(|argv| {
                let argv: Vec<String> = argv
                    .iter()
                    .filter_map(|arg| arg.as_str().map(str::to_string))
                    .collect();
                !is_probe(&argv)
            })
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

/// What `POST /v1/responses` answers. The default looks like a gateway that
/// supports Responses (missing `input`); the others classify as unsupported
/// (HTTP 405) and inconclusive (HTTP 500).
const RESPONSES_SUPPORTED: usize = 0;
const RESPONSES_UNSUPPORTED: usize = 1;
const RESPONSES_INCONCLUSIVE: usize = 2;

struct ProbeState {
    count: AtomicUsize,
    responses_mode: AtomicUsize,
}

async fn provider_models_handler(State(state): State<Arc<ProbeState>>) -> Json<Value> {
    state.count.fetch_add(1, Ordering::Relaxed);
    Json(serde_json::json!({
        "data": [{"id": "openai/gpt-5.3-codex", "context_length": 123456}],
    }))
}

async fn provider_responses_handler(
    State(state): State<Arc<ProbeState>>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state.count.fetch_add(1, Ordering::Relaxed);
    match state.responses_mode.load(Ordering::Relaxed) {
        RESPONSES_UNSUPPORTED => (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(serde_json::json!({"error": {"message": "method not allowed"}})),
        ),
        RESPONSES_INCONCLUSIVE => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": {"message": "upstream error"}})),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {"type": "invalid_request_error", "code": "missing_required_parameter", "message": "Missing required parameter: input"},
            })),
        ),
    }
}

async fn provider_unexpected_handler(
    State(state): State<Arc<ProbeState>>,
) -> (StatusCode, Json<Value>) {
    state.count.fetch_add(1, Ordering::Relaxed);
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "unexpected provider request"})),
    )
}

struct RequestCounter {
    base_url: String,
    state: Arc<ProbeState>,
    shutdown: Option<oneshot::Sender<()>>,
    _rt: Runtime,
}

impl RequestCounter {
    fn start() -> Self {
        let state = Arc::new(ProbeState {
            count: AtomicUsize::new(0),
            responses_mode: AtomicUsize::new(RESPONSES_SUPPORTED),
        });
        let rt = Builder::new_multi_thread().enable_all().build().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/models", get(provider_models_handler))
            .route("/v1/responses", post(provider_responses_handler))
            .fallback(provider_unexpected_handler)
            .with_state(state.clone());
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
            state,
            shutdown: Some(shutdown),
            _rt: rt,
        }
    }

    fn requests(&self) -> usize {
        self.state.count.load(Ordering::Relaxed)
    }

    fn set_responses_mode(&self, mode: usize) {
        self.state.responses_mode.store(mode, Ordering::Relaxed);
    }

    /// Close the listener so later requests fail to connect.
    fn stop(&mut self) {
        drop(self.shutdown.take());
        let addr = self
            .base_url
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(&addr).is_ok() {
            assert!(Instant::now() < deadline, "mock provider did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        !combined.contains("profile 'exec' not found"),
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
        argv.windows(2).any(|pair| pair[0] == "--profile"),
        "exec must receive a writable provider profile: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg.starts_with("model=")),
        "model must stay editable inside Codex"
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

#[test]
fn provider_header_secrets_reach_child_environment_without_argv_or_profile_leaks() {
    let home = temp_home("provider-header-secrets");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let path = provider_toml(&home);
    let mut profile: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    profile.as_table_mut().unwrap().insert("codex_config".into(), toml::Value::try_from(vec![
        r#"model_providers.openrouter.http_headers={X-Api-Key="obsolete-secret",X-Other="other-secret"}"#,
        r#"model_providers.openrouter.http_headers.X-Api-Key="effective-secret""#,
    ]).unwrap());
    fs::write(&path, toml::to_string(&profile).unwrap()).unwrap();
    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "--", "exec", "hi"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let argv = last_non_version_argv(&log);
    for secret in ["obsolete-secret", "other-secret", "effective-secret"] {
        assert!(
            !argv.iter().any(|a| a.contains(secret)),
            "secret in argv: {secret}"
        );
    }
    let log: Value = serde_json::from_slice(&fs::read(&log).unwrap()).unwrap();
    let env = log.as_array().unwrap().last().unwrap()["header_env"]
        .as_object()
        .unwrap();
    assert!(env.values().any(|v| v == "effective-secret"));
    assert!(env.values().any(|v| v == "other-secret"));
    assert!(!env.values().any(|v| v == "obsolete-secret"));
    for entry in fs::read_dir(home.join(".codex")).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".config.toml")
        {
            let config = fs::read_to_string(path).unwrap();
            assert!(!config.contains("effective-secret"));
            assert!(!config.contains("other-secret"));
        }
    }
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
    let launched = recorded_launches(&log).pop().unwrap();
    let path = launched
        .codex_home
        .join(format!("{}.config.toml", native_profile_name(&launched)));
    let config: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_ne!(
        config["model"].as_str(),
        Some("one-shot"),
        "one-shot model must not replace the persistent model"
    );

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
    assert!(!combined.contains("profile 'exec' not found"), "{combined}");
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
    let mut unsupported: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let record = &mut unsupported["responses_support"]["openai/gpt-5.3-codex"];
    assert_eq!(record["support"].as_str(), Some("supported"));
    assert!(!record["fingerprint"].as_str().unwrap().is_empty());
    assert!(record["checked_at"].as_integer().unwrap() > 0);
    record["support"] = toml::Value::String("unsupported".into());
    fs::write(&path, toml::to_string(&unsupported).unwrap()).unwrap();

    // A saved denial is re-checked live before it can refuse a launch; this
    // endpoint answers like a Responses gateway, so the launch proceeds and the
    // record is refreshed to supported.
    let rechecked = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    assert!(
        rechecked.status.success(),
        "{}",
        String::from_utf8_lossy(&rechecked.stderr)
    );
    assert_eq!(
        server.requests(),
        requests_before_launch + 1,
        "a saved denial must cost exactly one live re-check"
    );
    assert_eq!(saved_support(&path).as_deref(), Some("supported"));

    // A boolean from an older version carries no connection identity or
    // timestamp and must not preserve a permanent denial, so it is not even
    // re-checked.
    unsupported["responses_support"]["openai/gpt-5.3-codex"] = toml::Value::Boolean(false);
    fs::write(path, toml::to_string(&unsupported).unwrap()).unwrap();
    let legacy = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    assert!(
        legacy.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    assert_eq!(server.requests(), requests_before_launch + 1);
    let _ = fs::remove_dir_all(home);
}

const PROBED_MODEL: &str = "openai/gpt-5.3-codex";

fn provider_toml(home: &Path) -> PathBuf {
    home.join(".codex-switch/providers/openrouter/provider.toml")
}

fn saved_support(path: &Path) -> Option<String> {
    let saved: toml::Value = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    saved
        .get("responses_support")?
        .get(PROBED_MODEL)?
        .get("support")?
        .as_str()
        .map(str::to_string)
}

/// Probe once against a supporting endpoint so the saved record carries the
/// provider's real fingerprint, then rewrite its verdict, age and optionally
/// its fingerprint.
fn seed_saved_verdict(
    home: &Path,
    fake_bin: &Path,
    log: &Path,
    support: &str,
    age_secs: u64,
    fingerprint: Option<&str>,
) {
    let probed = run(
        home,
        fake_bin,
        log,
        &["provider", "probe", "openrouter", "--model", PROBED_MODEL],
    );
    assert!(
        probed.status.success(),
        "{}",
        String::from_utf8_lossy(&probed.stderr)
    );
    let path = provider_toml(home);
    let mut saved: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let record = &mut saved["responses_support"][PROBED_MODEL];
    record["support"] = toml::Value::String(support.into());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    record["checked_at"] = toml::Value::Integer((now - age_secs) as i64);
    if let Some(fingerprint) = fingerprint {
        record["fingerprint"] = toml::Value::String(fingerprint.into());
    }
    fs::write(path, toml::to_string(&saved).unwrap()).unwrap();
}

fn launch_output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn launch_refuses_a_saved_denial_that_a_live_probe_confirms() {
    let home = temp_home("recheck-confirmed");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(&home, &fake_bin, &log, "unsupported", 60, None);
    server.set_responses_mode(RESPONSES_UNSUPPORTED);
    let requests = server.requests();
    let launches = recorded_launches(&log).len();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(!output.status.success());
    let text = launch_output_text(&output);
    assert!(text.contains("no Codex Responses channel"), "{text}");
    assert!(text.contains("fresh probe just confirmed"), "{text}");
    assert_eq!(server.requests(), requests + 1);
    assert_eq!(
        recorded_launches(&log).len(),
        launches,
        "no Codex was started"
    );
    assert_eq!(
        saved_support(&provider_toml(&home)).as_deref(),
        Some("unsupported")
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_proceeds_and_saves_when_the_live_probe_says_supported() {
    let home = temp_home("recheck-cleared");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(&home, &fake_bin, &log, "unsupported", 60, None);
    let requests = server.requests();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    assert_eq!(server.requests(), requests + 1);
    assert_eq!(
        saved_support(&provider_toml(&home)).as_deref(),
        Some("supported")
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_fails_open_and_drops_the_denial_when_the_probe_cannot_reach_the_provider() {
    let home = temp_home("recheck-network-failure");
    let (fake_bin, log) = install_fake_codex(&home);
    let mut server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(&home, &fake_bin, &log, "unsupported", 60, None);
    server.stop();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning") && stderr.contains("launching anyway"),
        "the unconfirmed denial must be reported on stderr: {stderr}"
    );
    assert_eq!(
        saved_support(&provider_toml(&home)),
        None,
        "an unconfirmed denial must not keep triggering"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_fails_open_when_the_live_probe_is_inconclusive() {
    let home = temp_home("recheck-inconclusive");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(&home, &fake_bin, &log, "unsupported", 60, None);
    server.set_responses_mode(RESPONSES_INCONCLUSIVE);

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("inconclusive"));
    assert_eq!(saved_support(&provider_toml(&home)), None);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_does_not_probe_for_a_saved_supported_verdict() {
    let home = temp_home("recheck-supported-cached");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(&home, &fake_bin, &log, "supported", 60, None);
    server.set_responses_mode(RESPONSES_UNSUPPORTED);
    let requests = server.requests();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    assert_eq!(server.requests(), requests, "no probe on the normal path");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_ignores_a_denial_older_than_the_retention_window() {
    let home = temp_home("recheck-expired");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    let eight_days = 8 * 24 * 60 * 60;
    seed_saved_verdict(&home, &fake_bin, &log, "unsupported", eight_days, None);
    server.set_responses_mode(RESPONSES_UNSUPPORTED);
    let requests = server.requests();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    assert_eq!(server.requests(), requests, "an expired record is ignored");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_ignores_a_denial_recorded_for_a_different_connection() {
    let home = temp_home("recheck-fingerprint");
    let (fake_bin, log) = install_fake_codex(&home);
    let server = RequestCounter::start();
    setup_provider_at(&home, &server.base_url);
    seed_saved_verdict(
        &home,
        &fake_bin,
        &log,
        "unsupported",
        60,
        Some("fingerprint-of-another-endpoint"),
    );
    server.set_responses_mode(RESPONSES_UNSUPPORTED);
    let requests = server.requests();

    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);

    assert!(output.status.success(), "{}", launch_output_text(&output));
    assert_eq!(
        server.requests(),
        requests,
        "a stale fingerprint is ignored"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_launches_share_user_resources_with_distinct_profiles() {
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
    for launch in &launches {
        assert_eq!(
            launch.codex_home,
            home.join(".codex"),
            "provider must retain the native resource root"
        );
        assert!(
            launch.argv.windows(2).any(|pair| pair[0] == "--profile"),
            "provider must select an independent writable profile"
        );
    }
    let profiles: Vec<_> = launches
        .iter()
        .map(|launch| {
            launch
                .argv
                .windows(2)
                .find(|pair| pair[0] == "--profile")
                .unwrap()[1]
                .clone()
        })
        .collect();
    assert_ne!(profiles[0], profiles[1]);
    assert_ne!(profiles[0], profiles[2]);
    assert_ne!(profiles[1], profiles[2]);
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
    assert_eq!(last_run.codex_home, second_home);
    assert_ne!(
        native_profile_name(last_run),
        native_profile_name(&recorded_launches(&log)[1])
    );
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
    let launch = recorded_launches(&log).last().unwrap().clone();
    let saved: toml::Value = toml::from_str(
        &fs::read_to_string(
            launch
                .codex_home
                .join(format!("{}.config.toml", native_profile_name(&launch))),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(saved["model"].as_str(), Some("deepseek/deepseek-r1-0528"));
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
    assert_eq!(launches[1].codex_home, launches.last().unwrap().codex_home);
    assert_ne!(
        native_profile_name(&launches[1]),
        native_profile_name(launches.last().unwrap())
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_preserves_native_resources_and_default_identity() {
    let home = temp_home("provider-native-resources");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let codex_home = home.join(".codex");
    let fixtures = [
        (
            "config.toml",
            "model_provider = \"openai\"\nmodel = \"default-model\"\n[mcp_servers.demo]\ncommand = \"demo\"\n",
        ),
        ("auth.json", "{\"fixture\":\"default-account\"}"),
        (".credentials.json", "{\"fixture\":\"mcp-token\"}"),
        ("skills/demo/SKILL.md", "fixture skill"),
        ("agents/demo.toml", "fixture agent"),
        ("hooks.json", "{}"),
        ("plugins/fixture/data.json", "{}"),
    ];
    for (name, content) in fixtures {
        let path = codex_home.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let output = run(&home, &fake_bin, &log, &["launch", "openrouter"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let launch = recorded_launches(&log).pop().unwrap();
    assert_eq!(launch.codex_home, codex_home);
    for (name, content) in fixtures {
        assert_eq!(
            fs::read_to_string(launch.codex_home.join(name)).unwrap(),
            content,
            "resource {name} must retain native contents"
        );
    }
    let config: toml::Value = toml::from_str(
        &fs::read_to_string(
            codex_home.join(format!("{}.config.toml", native_profile_name(&launch))),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        config.get("mcp_servers").is_none(),
        "inherit public config rather than snapshot it"
    );
    assert_ne!(config["model_provider"].as_str(), Some("openai"));
    assert!(!config.to_string().contains("sk-test-passthrough"));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_survives_launcher_crash_with_json_output() {
    let home = temp_home("provider-crash");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let done = home.join("child-completed");
    let mut launcher = command(&home, &fake_bin, &log, &["--json", "launch", "openrouter"])
        .env("CS_FAKE_CODEX_SLEEP", "1")
        .env("CS_FAKE_CODEX_STDOUT_BYTES", "200000")
        .env("CS_FAKE_CODEX_DONE", &done)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let launched = loop {
        if let Some(launch) = try_recorded_launches(&log).and_then(|mut all| all.pop()) {
            break launch;
        }
        assert!(Instant::now() < deadline, "Codex child did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    while !done.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        done.exists(),
        "provider must survive parent death and keep writing output"
    );
    assert!(
        launched
            .codex_home
            .join(format!("{}.config.toml", native_profile_name(&launched)))
            .exists()
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn provider_resume_keeps_model_selected_inside_codex() {
    let home = temp_home("provider-model-persistence");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[("CS_FAKE_CODEX_SESSION_ID", "model-edit")],
    );
    assert!(output.status.success());
    let launched = recorded_launches(&log).pop().unwrap();
    let path = launched
        .codex_home
        .join(format!("{}.config.toml", native_profile_name(&launched)));
    let mut config: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["model"] = "deepseek/deepseek-r1-0528".into();
    config
        .as_table_mut()
        .unwrap()
        .insert("model_reasoning_effort".into(), "low".into());
    fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let resumed = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter", "resume", "model-edit"],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let config: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(config["model"].as_str(), Some("deepseek/deepseek-r1-0528"));
    assert_eq!(config["model_reasoning_effort"].as_str(), Some("low"));
    let _ = fs::remove_dir_all(home);
}

// ── Codex app-server daemon ───────────────────────────────

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| arg.to_string()).collect()
}

/// Every `codex app-server …` invocation the fake recorded, in order.
fn daemon_argv(log: &Path) -> Vec<Vec<String>> {
    recorded_argv(log)
        .into_iter()
        .filter(|argv| argv.first().is_some_and(|first| first == "app-server"))
        .collect()
}

#[test]
fn use_restarts_a_running_app_server_daemon() {
    let home = temp_home("use-daemon-restart");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["use", "work"],
        &[("CS_FAKE_CODEX_DAEMON", "running")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        daemon_argv(&log),
        vec![
            strings(&["app-server", "daemon", "version"]),
            strings(&["app-server", "daemon", "restart"]),
        ]
    );
    assert!(stdout.contains("Switched to profile: work"), "{stdout}");
    assert!(
        stdout.contains("Restarted the Codex app-server daemon"),
        "{stdout}"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn use_does_not_restart_the_daemon_when_the_live_auth_is_unchanged() {
    let home = temp_home("use-daemon-unchanged");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let running = [("CS_FAKE_CODEX_DAEMON", "running")];

    let first = run_env(&home, &fake_bin, &log, &["use", "work"], &running);
    assert!(first.status.success());
    assert_eq!(daemon_argv(&log).len(), 2, "the first switch restarts");

    let second = run_env(&home, &fake_bin, &log, &["use", "work"], &running);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(second.status.success(), "{combined}");
    assert_eq!(
        daemon_argv(&log).len(),
        2,
        "re-selecting the live profile must not touch the daemon again"
    );
    assert!(combined.contains("Switched to profile: work"), "{combined}");
    assert!(!combined.contains("app-server daemon"), "{combined}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn use_leaves_a_stopped_app_server_daemon_alone() {
    let home = temp_home("use-daemon-stopped");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(&home, &fake_bin, &log, &["use", "work"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{combined}");
    assert_eq!(
        daemon_argv(&log),
        vec![strings(&["app-server", "daemon", "version"])],
        "a stopped daemon must not be started by a switch"
    );
    assert!(combined.contains("Switched to profile: work"), "{combined}");
    assert!(!combined.contains("app-server daemon"), "{combined}");
    let _ = fs::remove_dir_all(home);
}

#[test]
fn use_reports_a_failed_daemon_restart_without_failing_the_switch() {
    let home = temp_home("use-daemon-restart-fails");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["use", "work"],
        &[
            ("CS_FAKE_CODEX_DAEMON", "running"),
            ("CS_FAKE_CODEX_DAEMON_RESTART", "fail"),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("Switched to profile: work"), "{stdout}");
    assert!(
        stderr.contains("still holds the previous account")
            && stderr.contains("not managed by codex app-server daemon")
            && stderr.contains("codex app-server daemon restart"),
        "{stderr}"
    );
    assert!(
        home.join(".codex/auth.json").is_file(),
        "the switch itself must have happened"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_use_keeps_the_daemon_report_off_stdout() {
    let home = temp_home("use-daemon-json");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["--json", "use", "work"],
        &[("CS_FAKE_CODEX_DAEMON", "running")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    let json: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("stdout must stay one JSON document ({err}): {stdout}"));
    assert_eq!(json["ok"], true);
    assert_eq!(json["alias"], "work");
    assert_eq!(json["action"], "switched");
    assert!(
        stderr.contains("Restarted the Codex app-server daemon"),
        "{stderr}"
    );
    assert_eq!(daemon_argv(&log).len(), 2);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_provider_selects_embedded_mode_when_supported() {
    let home = temp_home("provider-no-daemon");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);

    for (support, passthrough, expected_count) in [
        ("1", vec!["hello"], 1),
        ("1", vec!["exec", "--json", "hello"], 1),
        ("0", vec!["hello"], 0),
        ("1", vec!["--no-daemon", "hello"], 1),
        ("1", vec!["--remote", "ws://127.0.0.1:1"], 0),
    ] {
        let mut args = vec!["launch", "openrouter", "--"];
        args.extend(passthrough.iter().copied());
        let output = run_env(
            &home,
            &fake_bin,
            &log,
            &args,
            &[("CS_FAKE_CODEX_NO_DAEMON", support)],
        );
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let argv = last_non_version_argv(&log);
        assert_eq!(
            argv.iter().filter(|arg| *arg == "--no-daemon").count(),
            expected_count,
            "provider must explicitly select embedded mode when available: {argv:?}"
        );
        assert!(argv.windows(2).any(|pair| pair[0] == "--profile"));
        if passthrough[0] == "exec" {
            assert_eq!(&argv[..2], ["--no-daemon", "exec"]);
            assert!(
                argv[2..]
                    .windows(2)
                    .any(|pair| { pair[0] == "-c" && pair[1].starts_with("model_provider=") })
            );
            assert_eq!(&argv[argv.len() - 2..], ["--json", "hello"]);
        } else {
            assert!(argv.ends_with(&strings(&passthrough)));
        }
    }
    assert!(daemon_argv(&log).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_runs_codex_without_the_shared_daemon_when_supported() {
    let home = temp_home("launch-no-daemon");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "--json", "review"],
        &[("CS_FAKE_CODEX_NO_DAEMON", "1")],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "--json", "review"]
    );
    assert!(
        daemon_argv(&log).is_empty(),
        "launch must not touch the shared daemon"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_rejects_old_codex_before_touching_live_auth_even_with_explicit_server() {
    let home = temp_home("launch-minimum-version");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let live_auth = home.join(".codex/auth.json");
    write_auth(&live_auth, "original@example.com", "acct_original");
    let original = fs::read(&live_auth).unwrap();

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["--json", "launch", "work", "--", "--no-daemon", "hello"],
        &[("CS_FAKE_CODEX_VERSION", "0.159.1")],
    );

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("expected one JSON error envelope ({error}): {stdout}"));
    assert_eq!(report["ok"], false);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("requires Codex 0.159.2 or newer")
    );
    assert_eq!(fs::read(live_auth).unwrap(), original);
    assert!(recorded_launches(&log).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn old_codex_is_rejected_before_provider_native_run_creation() {
    let home = temp_home("provider-minimum-version");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);
    let provider_before =
        fs::read(home.join(".codex-switch/providers/openrouter/provider.toml")).unwrap();

    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "openrouter"],
        &[("CS_FAKE_CODEX_VERSION", "0.158.9")],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires Codex 0.159.2 or newer"));
    assert_eq!(
        fs::read(home.join(".codex-switch/providers/openrouter/provider.toml")).unwrap(),
        provider_before
    );
    assert!(
        !home.join(".codex-switch/provider-runs").exists(),
        "provider preflight must run before native run creation"
    );
    assert!(recorded_launches(&log).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn doctor_reports_path_and_explicit_desktop_engine_versions_as_json() {
    let home = temp_home("doctor-codex-version");
    let (fake_bin, log) = install_fake_codex(&home);
    let output = run(
        &home,
        &fake_bin,
        &log,
        &[
            "--json",
            "doctor",
            "--desktop-codex",
            fake_bin
                .join(if cfg!(windows) { "codex.cmd" } else { "codex" })
                .to_str()
                .unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("expected one JSON report ({error}): {stdout}"));
    assert_eq!(report["ok"], true);
    assert_eq!(report["minimum_version"], "0.159.2");
    assert_eq!(report["path_cli"]["version"], "0.159.2");
    assert_eq!(report["desktop_codex"]["version"], "0.159.2");
    assert_eq!(report["versions_match"], true);
    assert!(
        report["path_cli"]["executable"]
            .as_str()
            .unwrap()
            .contains("codex")
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn doctor_unknown_version_is_a_single_json_report_with_failure_exit() {
    let home = temp_home("doctor-unknown-version");
    let (fake_bin, log) = install_fake_codex(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["--json", "doctor"],
        &[("CS_FAKE_CODEX_VERSION_FAIL", "1")],
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("expected one JSON report ({error}): {stdout}"));
    assert_eq!(report["ok"], false);
    assert_eq!(report["path_cli"]["status"], "unknown");
    assert_eq!(report["path_cli"]["version"], Value::Null);
    assert!(
        report["path_cli"]["note"]
            .as_str()
            .unwrap()
            .contains("exited with 7")
    );
    assert!(String::from_utf8_lossy(&output.stderr).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn auto_launch_checks_version_before_reset_card_selection_or_auth_staging() {
    let home = temp_home("launch-auto-version-gate");
    let (fake_bin, log) = install_fake_codex(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["--json", "launch", "--consume-card"],
        &[("CS_FAKE_CODEX_VERSION", "0.159.1")],
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("expected one JSON error envelope ({error}): {stdout}"));
    assert_eq!(report["ok"], false);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("requires Codex 0.159.2 or newer")
    );
    assert!(!home.join(".codex/auth.json").exists());
    assert!(!home.join(".codex-switch/provider-runs").exists());
    assert!(recorded_launches(&log).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_accepts_a_slow_version_probe_within_the_budget() {
    // A cold Windows `codex.cmd` start can take several seconds; 6 s used to
    // exceed the 4 s probe budget and abort the launch.
    let home = temp_home("launch-slow-version");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[
            ("CS_FAKE_CODEX_VERSION_DELAY", "6"),
            ("CS_FAKE_CODEX_NO_DAEMON", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("could not verify"));
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "review"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_warns_and_continues_when_the_version_cannot_be_verified() {
    let home = temp_home("launch-unverifiable-version");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[
            ("CS_FAKE_CODEX_VERSION_FAIL", "1"),
            ("CS_FAKE_CODEX_NO_DAEMON", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not verify the Codex CLI version"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "review"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_accepts_a_slow_valid_help_probe() {
    let home = temp_home("launch-slow-help");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[
            ("CS_FAKE_CODEX_HELP_DELAY", "3"),
            ("CS_FAKE_CODEX_NO_DAEMON", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["--no-daemon", "exec", "review"]
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_rejects_failed_help_before_staging_credentials() {
    let home = temp_home("launch-failed-help");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);
    let live_auth = home.join(".codex/auth.json");
    write_auth(&live_auth, "original@example.com", "acct_original");
    let original = fs::read(&live_auth).unwrap();
    let output = run_env(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "review"],
        &[("CS_FAKE_CODEX_HELP_FAIL", "1")],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("account routing is unknown"));
    assert_eq!(fs::read(live_auth).unwrap(), original);
    assert!(recorded_launches(&log).is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_keeps_argv_for_a_codex_without_no_daemon() {
    let home = temp_home("launch-old-codex");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    let output = run(
        &home,
        &fake_bin,
        &log,
        &["launch", "work", "--", "exec", "--json", "review"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(last_non_version_argv(&log), ["exec", "--json", "review"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn launch_chatgpt_leaves_an_explicit_server_choice_alone() {
    let home = temp_home("launch-explicit-server");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_chatgpt(&home);

    for passthrough in [
        vec!["--remote", "ws://127.0.0.1:1"],
        vec!["--no-daemon", "hello"],
    ] {
        let mut args = vec!["launch", "work", "--"];
        args.extend(passthrough.iter().copied());
        let output = run_env(
            &home,
            &fake_bin,
            &log,
            &args,
            &[("CS_FAKE_CODEX_NO_DAEMON", "1")],
        );
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(last_non_version_argv(&log), strings(&passthrough));
    }
    let _ = fs::remove_dir_all(home);
}
