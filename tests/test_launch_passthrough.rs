#![cfg(unix)]

//! End-to-end argv contract for `codex-switch launch -- …`.
//!
//! A fake `codex` on PATH records the exact argument vector it received, so
//! these tests prove the composed command rather than only the clap parse.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

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

fn install_fake_codex(home: &Path) -> (PathBuf, PathBuf) {
    let bin_dir = home.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let log = home.join("fake-codex-log.json");
    fs::write(&log, "[]").unwrap();
    let script = bin_dir.join("codex");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, os, sys
path = os.environ["CS_FAKE_CODEX_LOG"]
try:
    data = json.loads(open(path, encoding="utf-8").read())
except Exception:
    data = []
data.append({"argv": sys.argv[1:]})
open(path, "w", encoding="utf-8").write(json.dumps(data))
if sys.argv[1:] == ["--version"]:
    sys.stdout.write("codex-cli 0.0.0-test\n")
else:
    size = int(os.environ.get("CS_FAKE_CODEX_STDOUT_BYTES", "0"))
    sys.stdout.write("x" * size if size else "codex-ok\n")
sys.exit(0)
"#,
    )
    .unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();
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

fn last_non_version_argv(log: &Path) -> Vec<String> {
    recorded_argv(log)
        .into_iter()
        .rev()
        .find(|argv| argv.as_slice() != ["--version"])
        .expect("fake codex must have been launched with real args")
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
    cmd.env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()));
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

fn run(home: &Path, fake_bin: &Path, log: &Path, args: &[&str]) -> Output {
    command(home, fake_bin, log, args).output().unwrap()
}

fn setup_provider_at(home: &Path, base_url: &str) {
    let dir = home.join(".codex-switch/providers/openrouter");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("provider.toml"),
        format!(
            r#"
provider_id = "openrouter"
name = "openrouter"
base_url = "{base_url}"
env_key = "CODEX_SWITCH_OPENROUTER_KEY"
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

fn setup_provider(home: &Path) {
    setup_provider_at(home, "http://127.0.0.1:9/v1");
}

struct RequestCounter {
    base_url: String,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RequestCounter {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_count = count.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut request = Vec::with_capacity(4096);
                        let mut chunk = [0_u8; 1024];
                        while request.len() < 16 * 1024 {
                            let n = stream.read(&mut chunk).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            request.extend_from_slice(&chunk[..n]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                        }
                        thread_count.fetch_add(1, Ordering::Relaxed);
                        let request = String::from_utf8_lossy(&request);
                        let (status, body) = if request.starts_with("POST /v1/responses ") {
                            (
                                "400 Bad Request",
                                r#"{"error":{"type":"invalid_request_error","message":"input required"}}"#,
                            )
                        } else {
                            (
                                "200 OK",
                                r#"{"data":[{"id":"openai/gpt-5.3-codex","context_length":123456}]}"#,
                            )
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            count,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

impl Drop for RequestCounter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
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
            "launch", "work", "--model", "gpt-5.4", "--", "exec", "--json", "hi",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        last_non_version_argv(&log),
        ["exec", "--model", "gpt-5.4", "--json", "hi"]
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
            "review this",
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
        ["--json", "--color", "never", "review this"]
    );
    assert!(
        !argv.iter().any(|a| a.contains("sk-test-passthrough")),
        "API key must not appear in argv: {argv:?}"
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
