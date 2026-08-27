#![cfg(unix)]

//! End-to-end argv contract for `codex-switch launch -- …`.
//!
//! A fake `codex` on PATH records the exact argument vector it received, so
//! these tests prove the composed command rather than only the clap parse.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    sys.stdout.write("codex-ok\n")
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

fn setup_provider(home: &Path) {
    let dir = home.join(".codex-switch/providers/openrouter");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("provider.toml"),
        r#"
provider_id = "openrouter"
name = "openrouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "CODEX_SWITCH_OPENROUTER_KEY"
default_model = "openai/gpt-5.3-codex"
wire_api = "responses"
api_key = "sk-test-passthrough"

[[models]]
id = "openai/gpt-5.3-codex"

[[models]]
id = "deepseek/deepseek-r1-0528"
reasoning = "high"
no_web_search = true
"#,
    )
    .unwrap();
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
    let home = temp_home("json-model");
    let (fake_bin, log) = install_fake_codex(&home);
    setup_provider(&home);

    let output = run(
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
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["alias"], "openrouter");
    assert_eq!(payload["model"], "one-shot");
    assert!(
        payload["codex_stdout"]
            .as_str()
            .is_some_and(|s| s.contains("codex-ok")),
        "codex stdout must be captured into the JSON envelope: {payload}"
    );
    let _ = fs::remove_dir_all(home);
}
