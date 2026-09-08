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
    cmd.env("CODEX_SWITCH_HOME", home.join(".codex-switch"));
    cmd.env_remove("HTTP_PROXY");
    cmd.env_remove("HTTPS_PROXY");
    cmd.env_remove("ALL_PROXY");
    cmd.env_remove("CS_PROXY");
    cmd
}

#[test]
fn spawned_binary_honors_app_home_override() {
    let home = temp_home("app-home-override");
    let app_home = home.join("isolated-app-home");
    let sample = home.join("sample-auth.json");
    write_json(
        &sample,
        &auth_json_needing_refresh("override@example.com", "acct_override"),
    );

    let mut cmd = command(
        &home,
        &["--json", "import", sample.to_str().unwrap(), "override"],
    );
    cmd.env("CODEX_SWITCH_HOME", &app_home);
    let server = start_rotating_mock(
        jwt(
            &serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_override"}}),
        ),
        true,
    );
    for (key, value) in import_env(&server) {
        cmd.env(key, value);
    }
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    assert!(app_home.join("profiles/override/auth.json").exists());

    let _ = fs::remove_dir_all(home);
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

fn quarantined_path_from_error(error: &str) -> PathBuf {
    let (_, after_path) = error
        .split_once("credential copy was quarantined at ")
        .expect("rotation rescue error must name its recovery path");
    let (path, _) = after_path
        .split_once(" and was not made into an activatable profile")
        .expect("rotation rescue error must end its recovery path before the profile warning");
    PathBuf::from(path)
}

fn assert_error_names_recovered_file(error: &str, recovered: &Path) {
    let reported = quarantined_path_from_error(error);
    assert_eq!(
        fs::canonicalize(&reported).unwrap(),
        fs::canonicalize(recovered).unwrap(),
        "the report must name the durable recovery path: {error}"
    );
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
fn json_use_rejects_untracked_live_auth_without_prompting() {
    let home = temp_home("json-use-untracked");
    write_json(
        home.join(".codex-switch/profiles/alice/auth.json"),
        &auth_json("alice@example.com", "acct_alice"),
    );
    write_json(
        home.join(".codex/auth.json"),
        &auth_json("bob@example.com", "acct_bob"),
    );

    let output = run(&home, &["--json", "use", "alice"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "current auth.json is not tracked; interactive confirmation is required before overwriting it"
        })
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("[y/N]"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_import_keeps_stdout_machine_readable() {
    let home = temp_home("json-import");
    let sample = home.join("sample-auth.json");
    write_json(
        &sample,
        &auth_json_needing_refresh("frank@example.com", "acct_frank"),
    );

    let server = start_rotating_mock(
        jwt(
            &serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_frank"}}),
        ),
        true,
    );
    let output = run_import(
        &home,
        &["--json", "import", sample.to_str().unwrap(), "frank"],
        &server,
    );
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "frank", "action": "created"})
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn import_skip_usage_validation_environment_variable_no_longer_bypasses_validation() {
    let home = temp_home("import-no-skip-bypass");
    let sample = home.join("sample-auth.json");
    write_json(
        &sample,
        &auth_json_with_access("victim@example.com", "acct_victim"),
    );
    let output = run_with_env(
        &home,
        &["--json", "import", sample.to_str().unwrap(), "victim"],
        &[("CS_IMPORT_SKIP_USAGE_VALIDATION", "1")],
    );
    assert!(
        !output.status.success(),
        "the retired environment escape hatch must not import credentials"
    );
    assert!(
        !home
            .join(".codex-switch/profiles/victim/auth.json")
            .exists()
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_reset_card_requires_explicit_yes() {
    let home = temp_home("json-reset-card-confirm");

    let output = run(&home, &["--json", "reset-card", "alice"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "confirmation required; rerun with --yes to consume a reset card"
        })
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
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("config.network.max_concurrent=0 is invalid; using 1 instead")
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn invalid_existing_config_fails_instead_of_using_defaults() {
    let home = temp_home("invalid-config");
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(
        home.join(".codex-switch/config.toml"),
        "[network]\nmax_concurrent = \"many\"\n",
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false);
    assert!(report["error"].as_str().unwrap().contains("config.toml"));
    assert!(report["error"].as_str().unwrap().contains("parse"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn invalid_config_error_does_not_echo_proxy_credentials() {
    let home = temp_home("invalid-config-secret");
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(
        home.join(".codex-switch/config.toml"),
        "[proxy]\nurl = \"http://user:SENTINEL_PASSWORD@example.com\n",
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("failed to parse config file"));
    assert!(!stdout.contains("SENTINEL_PASSWORD"));
    assert!(!stderr.contains("SENTINEL_PASSWORD"));

    let _ = fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn dangling_config_symlink_fails_instead_of_using_defaults() {
    use std::os::unix::fs::symlink;

    let home = temp_home("dangling-config-symlink");
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    symlink(
        home.join("missing-config.toml"),
        home.join(".codex-switch/config.toml"),
    )
    .unwrap();

    let output = run(&home, &["--json", "list"]);
    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false);
    assert!(report["error"].as_str().unwrap().contains("config.toml"));

    let _ = fs::remove_dir_all(home);
}

#[test]
fn json_delete_requires_explicit_yes_and_preserves_profile() {
    let home = temp_home("delete-json-confirm");
    write_json(
        home.join(".codex-switch/profiles/gina/auth.json"),
        &auth_json("gina@example.com", "acct_gina"),
    );

    let output = run(&home, &["--json", "delete", "gina"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "confirmation required; rerun with --yes to delete profile 'gina'"
        })
    );
    assert!(home.join(".codex-switch/profiles/gina/auth.json").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_interactive_delete_requires_explicit_yes_and_preserves_profile() {
    let home = temp_home("delete-non-interactive-confirm");
    write_json(
        home.join(".codex-switch/profiles/gina/auth.json"),
        &auth_json("gina@example.com", "acct_gina"),
    );

    let mut cmd = command(&home, &["delete", "gina"]);
    cmd.stdin(Stdio::null());
    let output = cmd.output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("confirmation required; rerun with --yes to delete profile 'gina'")
    );
    assert!(home.join(".codex-switch/profiles/gina/auth.json").exists());

    let _ = fs::remove_dir_all(home);
}

#[test]
fn delete_with_yes_archives_inactive_profile_for_recovery() {
    let home = temp_home("delete-yes");
    write_json(
        home.join(".codex-switch/profiles/gina/auth.json"),
        &auth_json("gina@example.com", "acct_gina"),
    );

    let output = run(&home, &["--json", "delete", "gina", "--yes"]);
    assert!(output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({"ok": true, "alias": "gina", "action": "deleted"})
    );
    assert!(!home.join(".codex-switch/profiles/gina").exists());
    let deleted_dir = home.join(".codex-switch/deleted-profiles");
    let archived: Vec<_> = fs::read_dir(&deleted_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(archived.len(), 1);
    assert!(
        archived[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("gina.backup-")
    );
    assert!(archived[0].join("auth.json").exists());

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

    let output = run(&home, &["delete", "gina", "--yes"]);
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
        &auth_json_needing_refresh("henry@example.com", "acct_henry"),
    );
    write_json(
        root.join("nested/invalid-structure.json"),
        &serde_json::json!({"tokens": {}}),
    );
    fs::create_dir_all(root.join("broken")).unwrap();
    fs::write(root.join("broken/not-json.json"), "{invalid json").unwrap();

    let server = start_rotating_mock(
        jwt(
            &serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct_henry"}}),
        ),
        true,
    );
    let output = run_import(
        &home,
        &["--json", "import", root.to_str().unwrap()],
        &server,
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
fn import_directory_returns_nonzero_when_every_file_is_skipped() {
    let home = temp_home("import-dir-all-skipped");
    let root = home.join("to-import");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("broken.json"), "{invalid json").unwrap();
    write_json(
        root.join("invalid-structure.json"),
        &serde_json::json!({"tokens": {}}),
    );

    let output = run(&home, &["--json", "import", root.to_str().unwrap()]);
    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["imported"].as_array().unwrap().len(), 0);
    assert_eq!(report["skipped"].as_array().unwrap().len(), 2);

    let _ = fs::remove_dir_all(home);
}

#[test]
fn non_json_all_skipped_import_reports_each_failure_before_exiting() {
    let home = temp_home("import-dir-all-skipped-details");
    let root = home.join("to-import");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("broken.json"), "{invalid json").unwrap();
    write_json(
        root.join("invalid-structure.json"),
        &serde_json::json!({"tokens": {}}),
    );

    let output = run(&home, &["import", root.to_str().unwrap()]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("broken.json [file_format]"), "{stdout}");
    assert!(
        stdout.contains("invalid-structure.json [structure]"),
        "{stdout}"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn automatic_use_without_profiles_explains_how_to_get_started() {
    let home = temp_home("use-no-profiles");

    let output = run(&home, &["--json", "use"]);
    assert!(!output.status.success());
    assert_eq!(
        parse_stdout_json(&output),
        serde_json::json!({
            "ok": false,
            "error": "no saved profiles; run `codex-switch login` or `codex-switch import <path>` first"
        })
    );

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

#[test]
fn warmup_rejects_path_traversal_alias_before_profile_use() {
    let home = temp_home("warmup-invalid-alias-preflight");
    let alias = "../escape";
    write_json(
        home.join(".codex-switch/escape/auth.json"),
        &auth_json("escape@example.com", "acct_escape"),
    );

    // The active cache keeps the command away from usage/network discovery;
    // the existing target path makes a missing alias preflight observable as
    // a false success.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    write_cache_entry(&home, alias, now, Some(10.0), Some(now as i64 + 3600));

    let output = run(&home, &["--json", "warmup", alias]);

    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    assert!(
        report["error"]
            .as_str()
            .unwrap_or_default()
            .contains("alias may only contain"),
        "path traversal must be rejected before the existing fixture can be used: {report}"
    );

    let _ = fs::remove_dir_all(home);
}

// ── import: rotated-credential rescue ─────────────────────
//
// OpenAI rotates `refresh_token` on every use and answers a replay with
// `refresh_token_reused`. During `import` the rotation happens *before* the
// usage validation call, so any failure after that point holds the only
// credential the auth server still accepts. These tests drive the real binary
// against a local auth/usage mock and assert the rotated token reaches disk.

struct MockServer {
    base_url: String,
    token_calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _rt: tokio::runtime::Runtime,
}

#[derive(Clone)]
struct MockState {
    rotated_id_token: String,
    /// Whether the usage endpoint answers the rotated access token or fails it.
    usage_ok: bool,
    token_calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Access tokens the usage endpoint fails regardless of `usage_ok` --
    /// lets one file in a directory import fail its usage check without
    /// forcing every other file in the same run to fail too.
    fail_access_tokens: std::collections::HashSet<String>,
}

async fn mock_usage_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let ok = state.usage_ok && !state.fail_access_tokens.contains(bearer);
    if ok {
        return (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 12.5,
                        "limit_window_seconds": 18_000,
                    }
                }
            })),
        );
    }
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({"detail": "upstream exploded"})),
    )
}

async fn mock_token_handler(
    axum::extract::State(state): axum::extract::State<MockState>,
    axum::Json(body): axum::Json<Value>,
) -> impl axum::response::IntoResponse {
    let presented = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    state.token_calls.lock().unwrap().push(presented.clone());
    if presented != "refresh_old" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "refresh_token_reused",
                "error_description": "replayed a consumed refresh token",
            })),
        );
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(serde_json::json!({
            "id_token": state.rotated_id_token,
            "access_token": "access_1",
            "refresh_token": "refresh_1",
        })),
    )
}

/// Auth server that rotates `refresh_old` -> `refresh_1` exactly once. With
/// `usage_ok` false every usage call then fails, reproducing "token rotated,
/// validation failed afterwards"; with it true the failure has to come from a
/// later step instead.
fn start_rotating_mock(rotated_id_token: String, usage_ok: bool) -> MockServer {
    start_rotating_mock_with_failures(rotated_id_token, usage_ok, &[])
}

/// Same as `start_rotating_mock`, but the usage endpoint also fails for any
/// bearer token listed in `fail_access_tokens`, independent of `usage_ok`.
/// Used to make exactly one file in a directory import fail its usage check
/// while a sibling file in the same run still succeeds normally.
fn start_rotating_mock_with_failures(
    rotated_id_token: String,
    usage_ok: bool,
    fail_access_tokens: &[&str],
) -> MockServer {
    let token_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let state = MockState {
        rotated_id_token,
        usage_ok,
        token_calls: token_calls.clone(),
        fail_access_tokens: fail_access_tokens.iter().map(|s| s.to_string()).collect(),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/backend-api/wham/usage",
            axum::routing::get(mock_usage_handler),
        )
        .route("/oauth/token", axum::routing::post(mock_token_handler))
        .with_state(state);
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    rt.spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    MockServer {
        base_url: format!("http://{addr}"),
        token_calls,
        _shutdown: shutdown,
        _rt: rt,
    }
}

fn expired_jwt() -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3600;
    jwt(&serde_json::json!({ "exp": exp }))
}

/// An auth.json whose access token already expired, so importing it forces the
/// proactive token refresh that consumes `refresh_old`.
fn auth_json_needing_refresh(email: &str, account_id: &str) -> Value {
    let mut value = auth_json(email, account_id);
    value["tokens"]["access_token"] = serde_json::json!(expired_jwt());
    value["tokens"]["refresh_token"] = serde_json::json!("refresh_old");
    value
}

fn import_env(server: &MockServer) -> Vec<(String, String)> {
    vec![
        (
            "CS_USAGE_URL".to_string(),
            format!("{}/backend-api/wham/usage", server.base_url),
        ),
        (
            "CS_TOKEN_URL".to_string(),
            format!("{}/oauth/token", server.base_url),
        ),
    ]
}

fn run_import(home: &Path, args: &[&str], server: &MockServer) -> Output {
    let env = import_env(server);
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    run_with_env(home, args, &pairs)
}

#[test]
fn import_rejects_an_invalid_alias_before_consuming_the_refresh_token() {
    let home = temp_home("import-invalid-alias-preflight");
    let sample = auth_json_needing_refresh("alias@example.com", "acct_alias");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);

    let server = start_rotating_mock(rotated_id_token, true);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "bad/name"],
        &server,
    );

    assert!(!output.status.success());
    assert!(
        server.token_calls.lock().unwrap().is_empty(),
        "a deterministic alias failure must be reported before a single-use refresh token is sent"
    );
    assert!(
        !home.join(".codex-switch/recovery").exists(),
        "a preflight rejection must not need credential recovery"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn import_rejects_a_disallowed_workspace_before_consuming_the_refresh_token() {
    let home = temp_home("import-managed-policy-preflight");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        "forced_chatgpt_workspace_id = \"acct_allowed\"\n",
    )
    .unwrap();
    let sample = auth_json_needing_refresh("blocked@example.com", "acct_blocked");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);

    let server = start_rotating_mock(rotated_id_token, true);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "blocked"],
        &server,
    );

    assert!(!output.status.success());
    assert!(
        server.token_calls.lock().unwrap().is_empty(),
        "managed policy must reject the source identity before its refresh token is sent"
    );
    let report = parse_stdout_json(&output);
    assert!(
        report["error"]
            .as_str()
            .unwrap_or_default()
            .contains("managed_policy"),
        "the rejection must identify the policy boundary: {report}"
    );

    let _ = fs::remove_dir_all(home);
}

fn stored_refresh_token(path: &Path) -> String {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {} failed: {e}", path.display()));
    let val: Value = serde_json::from_str(&raw).unwrap();
    val["tokens"]["refresh_token"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The rotation happens inside usage validation; when validation then fails the
/// only credential the auth server still accepts lives in memory. Dropping it
/// leaves the source file holding a token that can never be redeemed again, so
/// it has to be written somewhere durable before the failure is reported.
#[test]
fn import_persists_rotated_credentials_when_usage_validation_fails() {
    let home = temp_home("import-rotated-persist");
    let sample = auth_json_needing_refresh("rotate@example.com", "acct_rotate");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);

    let server = start_rotating_mock(rotated_id_token, false);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "donor"],
        &server,
    );

    assert!(
        !output.status.success(),
        "usage validation failed, so the import must not report success"
    );
    assert_eq!(
        server.token_calls.lock().unwrap().clone(),
        vec!["refresh_old".to_string()],
        "a consumed refresh token must never be replayed"
    );
    assert_eq!(
        stored_refresh_token(&home.join(".codex-switch/profiles/donor/auth.json")),
        "refresh_1",
        "the rotated refresh token was dropped; the account can no longer authenticate"
    );
    let report = parse_stdout_json(&output);
    assert_eq!(report["ok"], false, "stdout must stay machine-readable");
    let error = report["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("token_rotated"),
        "the failure must be tagged as a rotation rescue, got: {error}"
    );
    assert!(
        error.contains("donor"),
        "the failure must name where the rotated credentials were saved: {error}"
    );

    let _ = fs::remove_dir_all(home);
}

#[test]
fn import_quarantines_rotated_credentials_when_refresh_loses_account_identity() {
    let home = temp_home("import-rotated-missing-identity");
    let mut sample = auth_json("rotate@example.com", "acct_rotate");
    sample["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("access_token");
    sample["tokens"]
        .as_object_mut()
        .unwrap()
        .remove("account_id");
    sample["tokens"]["refresh_token"] = serde_json::json!("refresh_old");
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);

    let rotated_id_token = jwt(&serde_json::json!({
        "email": "rotate@example.com",
        "exp": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() + 3600,
    }));
    let server = start_rotating_mock(rotated_id_token, false);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "donor"],
        &server,
    );

    assert!(!output.status.success());
    assert_eq!(
        server.token_calls.lock().unwrap().clone(),
        vec!["refresh_old".to_string()]
    );
    let recovery_dir = home.join(".codex-switch/recovery");
    let recovered: Vec<_> = fs::read_dir(&recovery_dir)
        .expect("rotated credentials must be quarantined even without an account id")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(recovered.len(), 1);
    assert_eq!(stored_refresh_token(&recovered[0]), "refresh_1");
    assert!(
        !home.join(".codex-switch/profiles/donor").exists(),
        "unverified recovery credentials must not become an activatable profile"
    );
    let report = parse_stdout_json(&output);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(error.contains("token_rotated"));
    assert_error_names_recovered_file(error, &recovered[0]);

    let _ = fs::remove_dir_all(home);
}

/// A directory import prints one line per file. A file whose credentials were
/// rotated is not an ordinary skip — the source file is now worthless and a
/// profile appeared — so it must not be rendered like the others.
#[test]
fn import_directory_distinguishes_rotated_credentials_from_plain_skips() {
    let home = temp_home("import-rotated-dir");
    let root = home.join("to-import");
    let sample = auth_json_needing_refresh("dirrotate@example.com", "acct_dirrotate");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    write_json(root.join("rotating.json"), &sample);
    write_json(
        root.join("invalid-structure.json"),
        &serde_json::json!({"tokens": {}}),
    );

    let server = start_rotating_mock(rotated_id_token, false);
    let output = run_import(&home, &["import", root.to_str().unwrap()], &server);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rotated_line = stdout
        .lines()
        .find(|line| line.contains("rotating.json"))
        .unwrap_or_else(|| panic!("no line reported the rotated file:\n{stdout}"));
    let skipped_line = stdout
        .lines()
        .find(|line| line.contains("invalid-structure.json"))
        .unwrap_or_else(|| panic!("no line reported the invalid file:\n{stdout}"));

    assert!(
        skipped_line.contains("Skip"),
        "an ordinary validation failure stays a plain skip: {skipped_line}"
    );
    assert!(
        !rotated_line.contains("Skip"),
        "a rotated credential must not be reported as an ordinary skip: {rotated_line}"
    );
    assert!(
        rotated_line.contains("Rotated"),
        "the rotated file needs its own marker: {rotated_line}"
    );
    assert!(
        stdout.contains("refresh token"),
        "the user must be told the source file's refresh token is now dead:\n{stdout}"
    );
    assert_eq!(
        stored_refresh_token(&home.join(".codex-switch/profiles/dirrotate/auth.json")),
        "refresh_1",
        "the rotated refresh token was dropped during a directory import"
    );

    let _ = fs::remove_dir_all(home);
}

/// The rescue cannot depend on the source file being writable: auth dumps are
/// routinely copied in read-only, and a rescue that silently no-ops there is
/// exactly the data loss this guards against.
#[cfg(unix)]
#[test]
fn import_persists_rotated_credentials_when_source_file_is_read_only() {
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home("import-rotated-readonly");
    let sample = auth_json_needing_refresh("readonly@example.com", "acct_readonly");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let dir = home.join("readonly");
    let source = dir.join("donor-auth.json");
    write_json(&source, &sample);
    fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    let server = start_rotating_mock(rotated_id_token, false);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "ro"],
        &server,
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!output.status.success());
    assert_eq!(
        stored_refresh_token(&home.join(".codex-switch/profiles/ro/auth.json")),
        "refresh_1",
        "a read-only source must not cost the account its rotated credential"
    );
    let report = parse_stdout_json(&output);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("token_rotated"),
        "the rescue must still be reported, got: {error}"
    );

    let _ = fs::remove_dir_all(home);
}

/// If even the profile store cannot take the rotated credential there is
/// nothing left to save it into. That is the worst case and the one that must
/// never be quiet: the account is gone unless the user is told now.
#[test]
fn import_reports_loudly_when_rotated_credentials_cannot_be_saved() {
    let home = temp_home("import-rotated-lost");
    let sample = auth_json_needing_refresh("lost@example.com", "acct_lost");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);
    // Occupy both durable destinations with regular files so profile and
    // quarantine writes fail deterministically, with no permission-bit
    // semantics involved.
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/profiles"), "not a directory").unwrap();
    fs::write(home.join(".codex-switch/recovery"), "not a directory").unwrap();

    let server = start_rotating_mock(rotated_id_token, false);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "lost"],
        &server,
    );

    assert!(!output.status.success());
    let report = parse_stdout_json(&output);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("token_rotation_lost"),
        "an unsaveable rotation needs its own stage, got: {error}"
    );
    assert!(
        error.contains("sign in again"),
        "the user must learn the account needs a new login: {error}"
    );

    let _ = fs::remove_dir_all(home);
}

/// The same single-use credential is at stake when validation *succeeds* and
/// the profile write is what fails. A separate recovery store is still
/// available, so the rotated token must be quarantined instead of discarded.
#[test]
fn import_quarantines_the_rotation_when_the_profile_write_fails_after_validation() {
    let home = temp_home("import-rotated-save-failed");
    let sample = auth_json_needing_refresh("savefail@example.com", "acct_savefail");
    let rotated_id_token = sample["tokens"]["id_token"].as_str().unwrap().to_string();
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);
    fs::create_dir_all(home.join(".codex-switch")).unwrap();
    fs::write(home.join(".codex-switch/profiles"), "not a directory").unwrap();

    let server = start_rotating_mock(rotated_id_token, true);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "savefail"],
        &server,
    );

    assert!(!output.status.success());
    assert_eq!(
        server.token_calls.lock().unwrap().clone(),
        vec!["refresh_old".to_string()],
        "the scenario only holds if the credential really was rotated"
    );
    let report = parse_stdout_json(&output);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("token_rotated"),
        "a recoverable profile write failure must be reported as quarantined: {error}"
    );
    let recovered: Vec<_> = fs::read_dir(home.join(".codex-switch/recovery"))
        .expect("the rotated credential must fall back to the recovery store")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(recovered.len(), 1);
    assert_eq!(stored_refresh_token(&recovered[0]), "refresh_1");
    assert_error_names_recovered_file(error, &recovered[0]);

    let _ = fs::remove_dir_all(home);
}

/// The structure check that runs *after* usage validation inspects the value
/// the auth server just rotated, not the file on disk. A malformed refresh
/// reply therefore fails a check whose failure arrives when `refresh_old` is
/// already spent — so reporting it as an ordinary structure error would drop
/// the only credential the server still accepts.
#[test]
fn import_rescues_rotated_credentials_when_the_refreshed_value_is_malformed() {
    let home = temp_home("import-rotated-structure");
    let sample = auth_json_needing_refresh("badid@example.com", "acct_badid");
    let source = home.join("donor-auth.json");
    write_json(&source, &sample);

    // Usage succeeds, so the only thing that can fail afterwards is the
    // structure check — and it fails because the rotation handed back an
    // id_token that is not a JWT.
    let server = start_rotating_mock("not-a-jwt".to_string(), true);
    let output = run_import(
        &home,
        &["--json", "import", source.to_str().unwrap(), "donor"],
        &server,
    );

    assert!(
        !output.status.success(),
        "the refreshed credentials are malformed, so the import must not succeed"
    );
    assert_eq!(
        server.token_calls.lock().unwrap().clone(),
        vec!["refresh_old".to_string()],
        "the scenario only holds if the credential really was rotated"
    );
    assert_eq!(
        stored_refresh_token(&home.join(".codex-switch/profiles/donor/auth.json")),
        "refresh_1",
        "the rotated refresh token was dropped; the account can no longer authenticate"
    );
    let report = parse_stdout_json(&output);
    let error = report["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("token_rotated"),
        "the failure must be tagged as a rotation rescue, got: {error}"
    );

    let _ = fs::remove_dir_all(home);
}

/// A `--json` directory import that imports one file fine but loses another
/// account's rotated credentials (`token_rotation_lost`) must not read as an
/// overall success: a script that only checks a top-level discovery field,
/// without walking `skipped[]`, still has to learn an account needs a fresh
/// login.
#[test]
fn json_directory_import_surfaces_lost_credentials_at_top_level() {
    let home = temp_home("import-json-lost-visible");
    let root = home.join("to-import");

    let lost_sample = auth_json_needing_refresh("lost@example.com", "acct_lost");
    let rotated_id_token = lost_sample["tokens"]["id_token"]
        .as_str()
        .unwrap()
        .to_string();
    write_json(root.join("lost-auth.json"), &lost_sample);
    write_json(
        root.join("ok-auth.json"),
        &auth_json_with_access("healthy@example.com", "acct_healthy"),
    );

    // The alias the "lost" identity would rescue into ("lost", derived from
    // its email) already exists as a plain file instead of a directory, so
    // creating its profile subdirectory fails -- unlike a permission bit,
    // this can't be silently repaired by `ensure_private_dir`'s chmod. The
    // rest of the profile store stays untouched and writable for
    // "ok-auth.json".
    fs::create_dir_all(home.join(".codex-switch/profiles")).unwrap();
    fs::write(home.join(".codex-switch/profiles/lost"), "not a directory").unwrap();
    fs::write(home.join(".codex-switch/recovery"), "not a directory").unwrap();

    let server = start_rotating_mock_with_failures(rotated_id_token, true, &["access_1"]);
    let output = run_import(
        &home,
        &["--json", "import", root.to_str().unwrap()],
        &server,
    );

    let report = parse_stdout_json(&output);
    assert_eq!(
        report["imported"].as_array().unwrap().len(),
        1,
        "the healthy file must still import normally: {report}"
    );
    let skipped = report["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|item| item["stage"] == "token_rotation_lost"),
        "expected a token_rotation_lost entry in skipped[]: {report}"
    );
    assert_eq!(
        report["credentials_lost"],
        serde_json::json!(true),
        "a lost account must be discoverable from a top-level field alone, without walking \
         skipped[]: {report}"
    );

    let _ = fs::remove_dir_all(home);
}
