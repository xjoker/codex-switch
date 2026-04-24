use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-switch-{name}-{ts}-{id}"));
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

fn auth_json(email: &str, account_id: &str) -> Value {
    let claims = serde_json::json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus",
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
            "organizations": [],
        }
    });

    serde_json::json!({
        "tokens": {
            "id_token": jwt(&claims),
            "refresh_token": "dummy-refresh",
            "account_id": account_id,
        }
    })
}

fn auth_json_with_access(email: &str, account_id: &str) -> Value {
    let mut value = auth_json(email, account_id);
    value["tokens"]["access_token"] = serde_json::json!("dummy-access");
    value
}

fn write_json(path: impl AsRef<Path>, value: &Value) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn write_cache_entry(
    home: &Path,
    alias: &str,
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
) {
    let cache = serde_json::json!({
        "entries": {
            alias: {
                "ts": ts,
                "primary_used": primary_used,
                "primary_reset": primary_reset,
                "secondary_used": null,
                "secondary_reset": null
            }
        }
    });
    write_json(home.join(".codex-switch/cache.json"), &cache);
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codex-switch")
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(binary());
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.env("HOME", home);
    cmd.env("CODEX_HOME", home.join(".codex"));
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

fn run(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().unwrap()
}

fn run_with_env(home: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = command(home, args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

fn run_with_timeout(home: &Path, args: &[&str], timeout: Duration) -> Output {
    let mut child = command(home, args).spawn().unwrap();
    let start = std::time::Instant::now();

    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command timed out: {:?}", args);
        }

        thread::sleep(Duration::from_millis(20));
    }
}

fn parse_stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn json_use_keeps_stdout_machine_readable() {
    let home = temp_home("json-use");
    write_json(
        home.join(".codex-switch/profiles/alice/auth.json"),
        &auth_json("alice@example.com", "acct_alice"),
    );
    write_json(
        home.join(".codex/auth.json"),
        &auth_json("alice@example.com", "acct_alice"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "alice").unwrap();

    let output = run(&home, &["--json", "use", "alice"]);
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "alice", "action": "switched"})
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Switched to profile: alice"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_import_keeps_stdout_machine_readable() {
    let home = temp_home("json-import");
    let sample = home.join("sample-auth.json");
    write_json(
        &sample,
        &auth_json_with_access("frank@example.com", "acct_frank"),
    );

    let output = run_with_env(
        &home,
        &["--json", "import", sample.to_str().unwrap(), "frank"],
        &[("CS_IMPORT_SKIP_USAGE_VALIDATION", "1")],
    );
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "frank", "action": "created"})
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_list_auto_track_keeps_stdout_machine_readable() {
    let home = temp_home("json-list");
    write_json(
        home.join(".codex/auth.json"),
        &auth_json("carol@example.com", "acct_carol"),
    );

    let output = run(&home, &["--json", "list"]);
    assert!(output.status.success());

    let stdout = parse_stdout_json(&output);
    assert_eq!(stdout["profiles"][0]["alias"], "carol");
    assert_eq!(
        stdout["profiles"][0]["usage"]["error"],
        "no access_token in auth file"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Saved profile: carol"));
    assert!(stderr.contains("Auto-saved current account as profile: carol"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn zero_max_concurrent_is_sanitized() {
    let home = temp_home("zero-max-concurrent");
    write_json(
        home.join(".codex-switch/profiles/dave/auth.json"),
        &auth_json("dave@example.com", "acct_dave"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "dave").unwrap();
    fs::write(
        home.join(".codex-switch/config.toml"),
        "[network]\nmax_concurrent = 0\n",
    )
    .unwrap();

    let output = run_with_timeout(&home, &["--json", "list"], Duration::from_secs(10));
    assert!(output.status.success());
    assert_eq!(parse_stdout_json(&output)["profiles"][0]["alias"], "dave");

    let _ = fs::remove_dir_all(home);
}

#[test]
fn delete_rejects_active_profile() {
    let home = temp_home("delete-active");
    write_json(
        home.join(".codex-switch/profiles/gina/auth.json"),
        &auth_json("gina@example.com", "acct_gina"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "gina").unwrap();

    let output = run(&home, &["delete", "gina"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot delete the active profile"));
    assert!(home.join(".codex-switch/profiles/gina/auth.json").exists());
    assert_eq!(
        fs::read_to_string(home.join(".codex-switch/current")).unwrap(),
        "gina"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn import_directory_recursively_validates_and_reports_results() {
    let home = temp_home("import-dir");
    let root = home.join("to-import");
    write_json(
        root.join("nested/valid-auth.json"),
        &auth_json_with_access("henry@example.com", "acct_henry"),
    );
    write_json(
        root.join("nested/invalid-structure.json"),
        &serde_json::json!({"tokens": {}}),
    );
    fs::create_dir_all(root.join("broken")).unwrap();
    fs::write(root.join("broken/not-json.json"), "{invalid json").unwrap();

    let output = run_with_env(
        &home,
        &["--json", "import", root.to_str().unwrap()],
        &[("CS_IMPORT_SKIP_USAGE_VALIDATION", "1")],
    );
    assert!(output.status.success());

    let report = parse_stdout_json(&output);
    assert_eq!(report["imported"].as_array().unwrap().len(), 1);
    assert_eq!(report["imported"][0]["alias"], "henry");
    assert_eq!(report["skipped"].as_array().unwrap().len(), 2);

    assert!(home.join(".codex-switch/profiles/henry/auth.json").exists());
    let skipped_stages: Vec<_> = report["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["stage"].as_str().unwrap())
        .collect();
    assert!(skipped_stages.contains(&"file_format"));
    assert!(skipped_stages.contains(&"structure"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_list_uses_per_account_cached_refresh_time() {
    let home = temp_home("json-list-cache-ts");
    write_json(
        home.join(".codex-switch/profiles/ivy/auth.json"),
        &auth_json("ivy@example.com", "acct_ivy"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "ivy").unwrap();
    fs::write(
        home.join(".codex-switch/config.toml"),
        "[cache]\nttl = 999999999\n",
    )
    .unwrap();

    write_cache_entry(&home, "ivy", 1_710_000_000, Some(42.0), Some(1_710_001_800));

    let output = run(&home, &["--json", "list"]);
    assert!(output.status.success());

    let stdout = parse_stdout_json(&output);
    assert_eq!(stdout["profiles"][0]["alias"], "ivy");
    assert_eq!(
        stdout["profiles"][0]["usage"]["primary"]["used_percent"],
        42.0
    );
    assert_eq!(
        stdout["profiles"][0]["usage"]["fetched_at"],
        "2024-03-09T16:00:00Z"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_stdin_does_not_save_new_account() {
    let home = temp_home("non-interactive-new");
    // Put an auth.json with no matching profile
    write_json(
        home.join(".codex/auth.json"),
        &auth_json("notrack@example.com", "acct_notrack"),
    );

    // Non-JSON, stdin closed: startup check should detect NewAccount but NOT save
    let output = command(&home, &["list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should inform user about the new account (user_println goes to stdout in non-JSON mode)
    assert!(
        stdout.contains("Detected new account"),
        "expected detection message in stdout, got: {stdout}"
    );
    // Should NOT have saved — no profiles directory should exist
    // (auto_track_current is skipped because auth_already_handled=true)
    let profiles_dir = home.join(".codex-switch/profiles");
    assert!(
        !profiles_dir.exists() || fs::read_dir(&profiles_dir).unwrap().count() == 0,
        "expected no profiles saved, but profiles dir has content"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_stdin_does_not_update_existing_profile() {
    let home = temp_home("non-interactive-update");
    // Create profile for alice
    write_json(
        home.join(".codex-switch/profiles/alice/auth.json"),
        &auth_json("alice@example.com", "acct_alice"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "alice").unwrap();

    // Put updated auth.json (same identity, different tokens) in live location
    let mut updated = auth_json("alice@example.com", "acct_alice");
    updated["tokens"]["refresh_token"] = serde_json::json!("new-refresh-token");
    updated["tokens"]["access_token"] = serde_json::json!("new-access-token");
    write_json(home.join(".codex/auth.json"), &updated);

    // Run with stdin closed — should detect but NOT update profile
    let output = command(&home, &["list"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("credentials changed"),
        "expected change detection message in stdout, got: {stdout}"
    );

    // Profile file should still have the original content (not updated)
    let profile_content: Value = serde_json::from_str(
        &fs::read_to_string(home.join(".codex-switch/profiles/alice/auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        profile_content["tokens"]["refresh_token"], "dummy-refresh",
        "profile refresh_token should not have been updated"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn list_progress_counts_only_stale_accounts() {
    let home = temp_home("list-progress-stale-only");
    write_json(
        home.join(".codex-switch/profiles/fresh/auth.json"),
        &auth_json("fresh@example.com", "acct_fresh"),
    );
    write_json(
        home.join(".codex-switch/profiles/stale/auth.json"),
        &auth_json("stale@example.com", "acct_stale"),
    );
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/current"), "fresh").unwrap();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_cache_entry(&home, "fresh", now, Some(10.0), Some(now as i64 + 3600));

    let output = run_with_env(&home, &["list"], &[("CS_PROGRESS_FORCE", "1")]);
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Refreshing usage ["));
    assert!(stderr.contains("1/1"));

    let _ = fs::remove_dir_all(home);
}
