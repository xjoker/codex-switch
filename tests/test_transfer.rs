//! End-to-end tests for `codex-switch export` / `import <bundle>`.
//!
//! Every test spawns the real binary against isolated HOME / CODEX_HOME /
//! CODEX_SWITCH_HOME directories ("machines") and uses fake JWTs only.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{Value, json};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const PASS: &str = "correct horse battery staple";

fn temp_home(name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-switch-transfer-{name}-{ts}-{id}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn jwt(payload: &Value) -> String {
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(payload).unwrap());
    format!("x.{encoded}.y")
}

fn auth(email: &str, account_id: &str, tag: &str, last_refresh: &str) -> Value {
    let claims = json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus",
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
        }
    });
    json!({
        "tokens": {
            "id_token": jwt(&claims),
            "access_token": format!("access-{tag}"),
            "refresh_token": format!("refresh-{tag}"),
            "account_id": account_id,
        },
        "last_refresh": last_refresh,
    })
}

fn app(home: &Path) -> PathBuf {
    home.join(".codex-switch")
}

fn write(path: impl AsRef<Path>, text: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn put_profile(home: &Path, alias: &str, value: &Value) {
    write(
        app(home).join("profiles").join(alias).join("auth.json"),
        &serde_json::to_string_pretty(value).unwrap(),
    );
}

fn provider_toml(alias: &str, identity: &str, key: &str, model: &str) -> String {
    format!(
        r#"identity_id = "{identity}"
provider_id = "{alias}"
name = "{alias}"
base_url = "https://example.invalid/v1"
env_key = "CS_TEST_PROVIDER_KEY"
default_model = "{model}"
api_key = "{key}"

[[models]]
id = "{model}"
"#
    )
}

const CONFIG_A: &str =
    "[proxy]\nurl = \"http://user:proxy-secret-pw@127.0.0.1:9\"\n\n[cache]\nttl = 120\n";

/// Machine A: two profiles (work is current), one provider with models.json,
/// and a config.toml carrying a proxy password.
fn machine_a() -> PathBuf {
    let home = temp_home("a");
    put_profile(
        &home,
        "work",
        &auth(
            "work@example.com",
            "acct-work",
            "work-a",
            "2026-10-10T00:00:00Z",
        ),
    );
    put_profile(
        &home,
        "personal",
        &auth("me@example.com", "acct-me", "me-a", "2026-10-10T00:00:00Z"),
    );
    write(app(&home).join("current"), "work");
    write(
        app(&home).join("providers/openrouter/provider.toml"),
        &provider_toml(
            "openrouter",
            "11111111-1111-4111-8111-111111111111",
            "sk-provider-secret",
            "m1",
        ),
    );
    write(
        app(&home).join("providers/openrouter/models.json"),
        "{\"models\":[]}\n",
    );
    write(app(&home).join("config.toml"), CONFIG_A);
    home
}

fn command(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-switch"));
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("CODEX_SWITCH_HOME", app(home))
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("CS_PROXY")
        .env_remove("CODEX_SWITCH_TEST_IMPORT_FAIL_AT");
    cmd
}

fn run_with_stdin(mut cmd: Command, stdin: &str) -> Output {
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let cmd = command(home, args);
    match stdin {
        Some(input) => run_with_stdin(cmd, input),
        None => {
            let mut cmd = cmd;
            cmd.output().unwrap()
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn export(home: &Path, dest: &Path) {
    let out = run(
        home,
        &["export", dest.to_str().unwrap(), "--passphrase-stdin"],
        Some(&format!("{PASS}\n")),
    );
    assert!(out.status.success(), "{}", text(&out));
}

fn import(home: &Path, bundle: &Path, extra: &[&str], pass: &str) -> Output {
    let mut args = vec!["import", bundle.to_str().unwrap(), "--passphrase-stdin"];
    args.extend_from_slice(extra);
    run(home, &args, Some(&format!("{pass}\n")))
}

fn json_import(home: &Path, bundle: &Path, extra: &[&str], pass: &str) -> (Output, Value) {
    let mut args = vec![
        "--json",
        "import",
        bundle.to_str().unwrap(),
        "--passphrase-stdin",
    ];
    args.extend_from_slice(extra);
    let out = run(home, &args, Some(&format!("{pass}\n")));
    let value: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {}", text(&out)));
    (out, value)
}

/// Snapshot of every file under the app home, ignoring logs and lock files,
/// which any command touches.
fn tree(home: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(read) = fs::read_dir(dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with(".codex-switch/logs") || rel.ends_with(".lock") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(home, home, &mut out);
    out
}

fn items(v: &Value) -> Vec<(String, String, String)> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["key"].as_str().unwrap().to_string(),
                i["action"].as_str().unwrap().to_string(),
                i["target"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect()
}

fn action_of(v: &Value, key: &str) -> String {
    items(v)
        .into_iter()
        .find(|(k, _, _)| k == key)
        .map(|(_, a, _)| a)
        .unwrap_or_else(|| panic!("no item {key} in {v}"))
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn bundle_of(name: &str) -> (PathBuf, PathBuf) {
    let a = machine_a();
    let bundle = temp_home(name).join("out.csbundle");
    export(&a, &bundle);
    (a, bundle)
}

// 1
#[test]
fn round_trip_into_empty_machine() {
    let (a, bundle) = bundle_of("rt");
    let b = temp_home("b");
    let out = import(&b, &bundle, &[], PASS);
    assert!(out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("codex-switch use work"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("single-use"), "{stderr}");

    for rel in [
        "profiles/work/auth.json",
        "profiles/personal/auth.json",
        "providers/openrouter/provider.toml",
        "providers/openrouter/models.json",
        "config.toml",
    ] {
        assert_eq!(
            fs::read(app(&a).join(rel)).unwrap(),
            fs::read(app(&b).join(rel)).unwrap(),
            "{rel} differs"
        );
    }
    assert!(
        !app(&b).join("current").exists(),
        "current must not be written"
    );
    assert!(!b.join(".codex/auth.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(app(&b).join("profiles/work/auth.json")), 0o600);
        assert_eq!(mode(app(&b).join("profiles/work")), 0o700);
        assert_eq!(
            mode(app(&b).join("providers/openrouter/provider.toml")),
            0o600
        );
        assert_eq!(mode(bundle.clone()), 0o600);
    }
}

// 2
#[test]
fn wrong_passphrase_is_distinct_and_writes_nothing() {
    let (_a, bundle) = bundle_of("wp");
    let b = temp_home("b");
    let before = tree(&b);
    let (out, v) = json_import(&b, &bundle, &[], "not the passphrase");
    assert!(!out.status.success());
    assert_eq!(v["code"], "wrong_passphrase", "{v}");
    assert_eq!(before, tree(&b));
}

fn edit_bundle(bundle: &Path, f: impl FnOnce(&mut Value)) -> PathBuf {
    let mut v = read_json(bundle);
    f(&mut v);
    let out = bundle.with_file_name(format!(
        "edited-{}.csbundle",
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&out, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    out
}

// 3
#[test]
fn tampering_is_detected() {
    let (_a, bundle) = bundle_of("tamper");
    let b = temp_home("b");
    let before = tree(&b);

    let flipped = edit_bundle(&bundle, |v| {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut ct = b64.decode(v["ciphertext"].as_str().unwrap()).unwrap();
        ct[5] ^= 0x01;
        v["ciphertext"] = json!(b64.encode(ct));
    });
    let (_, v) = json_import(&b, &flipped, &[], PASS);
    assert_eq!(v["code"], "bundle_tampered", "{v}");

    let dated = edit_bundle(&bundle, |v| v["created_at"] = json!("2020-01-01T00:00:00Z"));
    let (_, v) = json_import(&b, &dated, &[], PASS);
    assert_eq!(v["code"], "bundle_tampered", "{v}");

    let salted = edit_bundle(&bundle, |v| {
        v["encryption"]["kdf"]["salt"] = json!("AAAAAAAAAAAAAAAAAAAAAA==")
    });
    let (_, v) = json_import(&b, &salted, &[], PASS);
    assert_eq!(v["code"], "wrong_passphrase", "{v}");

    assert_eq!(before, tree(&b));
}

// 4
#[test]
fn kdf_bounds_are_checked_before_derivation() {
    let (_a, bundle) = bundle_of("kdf");
    let b = temp_home("b");
    let huge = edit_bundle(&bundle, |v| {
        v["encryption"]["kdf"]["m_cost_kib"] = json!(4_194_304)
    });
    let started = std::time::Instant::now();
    let (out, v) = json_import(&b, &huge, &[], PASS);
    assert!(!out.status.success());
    assert_eq!(v["code"], "unsupported_encryption", "{v}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

/// Machine B already has `work` (same identity as A's) with other tokens.
fn machine_b_with_conflict(last_refresh: &str) -> PathBuf {
    let b = temp_home("b");
    put_profile(
        &b,
        "work",
        &auth("work@example.com", "acct-work", "work-b", last_refresh),
    );
    b
}

// 5
#[test]
fn strategy_skip_keeps_local_by_default() {
    let (_a, bundle) = bundle_of("skip");
    let b = machine_b_with_conflict("2026-10-01T00:00:00Z");
    let local = fs::read(app(&b).join("profiles/work/auth.json")).unwrap();
    let (out, v) = json_import(&b, &bundle, &[], PASS);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(v["strategy"], "skip");
    assert_eq!(action_of(&v, "profiles/work/auth.json"), "skip");
    assert_eq!(action_of(&v, "profiles/personal/auth.json"), "create");
    assert_eq!(
        fs::read(app(&b).join("profiles/work/auth.json")).unwrap(),
        local
    );
}

// 6
#[test]
fn strategy_overwrite_replaces_backs_up_and_updates_live() {
    let (a, bundle) = bundle_of("ow");
    let b = machine_b_with_conflict("2026-10-01T00:00:00Z");
    let local = fs::read(app(&b).join("profiles/work/auth.json")).unwrap();
    write(app(&b).join("current"), "work");
    write(
        b.join(".codex/auth.json"),
        &String::from_utf8(local.clone()).unwrap(),
    );

    // Without --yes a non-interactive overwrite is refused.
    let (out, _) = json_import(&b, &bundle, &["--strategy", "overwrite"], PASS);
    assert!(!out.status.success());
    assert_eq!(
        fs::read(app(&b).join("profiles/work/auth.json")).unwrap(),
        local
    );

    let (out, v) = json_import(&b, &bundle, &["--strategy", "overwrite", "--yes"], PASS);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(action_of(&v, "profiles/work/auth.json"), "overwrite");
    let imported = fs::read(app(&a).join("profiles/work/auth.json")).unwrap();
    assert_eq!(
        fs::read(app(&b).join("profiles/work/auth.json")).unwrap(),
        imported
    );
    assert_eq!(
        read_json(&b.join(".codex/auth.json")),
        read_json(&app(&a).join("profiles/work/auth.json"))
    );
    let backup = PathBuf::from(v["backup_dir"].as_str().unwrap());
    assert_eq!(
        fs::read(backup.join("backup/app/profiles/work/auth.json")).unwrap(),
        local
    );
    let journal = read_json(&backup.join("journal.json"));
    assert_eq!(journal["state"], "committed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            "",
            "backup",
            "backup/app",
            "backup/app/profiles",
            "backup/live",
        ] {
            let mode = fs::metadata(backup.join(dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "backup dir '{dir}'");
        }
    }
}

// 7
#[test]
fn strategy_merge_keeps_newer_credentials_and_local_config() {
    // (a) imported newer -> imported wins
    let (a, bundle) = bundle_of("merge");
    let b = machine_b_with_conflict("2026-10-01T00:00:00Z");
    write(app(&b).join("config.toml"), "[cache]\nttl = 999\n");
    write(
        app(&b).join("providers/openrouter/provider.toml"),
        &provider_toml(
            "openrouter",
            "11111111-1111-4111-8111-111111111111",
            "sk-local",
            "m2",
        ),
    );
    let (out, v) = json_import(&b, &bundle, &["--strategy", "merge", "--yes"], PASS);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(action_of(&v, "profiles/work/auth.json"), "overwrite");
    assert_eq!(
        fs::read(app(&b).join("profiles/work/auth.json")).unwrap(),
        fs::read(app(&a).join("profiles/work/auth.json")).unwrap()
    );
    let config = fs::read_to_string(app(&b).join("config.toml")).unwrap();
    assert_eq!(action_of(&v, "config.toml"), "merge");
    assert!(config.contains("ttl = 999"), "{config}");
    assert!(config.contains("proxy"), "{config}");
    // Provider merge is deferred: treated as skip.
    assert_eq!(action_of(&v, "providers/openrouter/provider.toml"), "skip");
    assert!(
        fs::read_to_string(app(&b).join("providers/openrouter/provider.toml"))
            .unwrap()
            .contains("sk-local")
    );

    // (b) imported older -> local kept
    let b2 = machine_b_with_conflict("2026-10-20T00:00:00Z");
    let local = fs::read(app(&b2).join("profiles/work/auth.json")).unwrap();
    let (out, v) = json_import(&b2, &bundle, &["--strategy", "merge", "--yes"], PASS);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(action_of(&v, "profiles/work/auth.json"), "skip");
    assert_eq!(
        fs::read(app(&b2).join("profiles/work/auth.json")).unwrap(),
        local
    );
}

// 8
#[test]
fn alias_collision_with_other_account_always_renames() {
    let (_a, bundle) = bundle_of("collide");
    for strategy in ["skip", "overwrite", "merge"] {
        let b = temp_home("b");
        put_profile(
            &b,
            "work",
            &auth(
                "other@example.com",
                "acct-other",
                "other",
                "2026-10-01T00:00:00Z",
            ),
        );
        // Case-only collision with a different account.
        put_profile(
            &b,
            "Personal",
            &auth("x@example.com", "acct-x", "x", "2026-10-01T00:00:00Z"),
        );
        let local_work = fs::read(app(&b).join("profiles/work/auth.json")).unwrap();
        let (out, v) = json_import(&b, &bundle, &["--strategy", strategy, "--yes"], PASS);
        assert!(out.status.success(), "{strategy}: {}", text(&out));
        let it = items(&v);
        assert!(
            it.contains(&(
                "profiles/work/auth.json".into(),
                "rename".into(),
                "work_2".into()
            )),
            "{strategy}: {v}"
        );
        assert!(
            it.contains(&(
                "profiles/personal/auth.json".into(),
                "rename".into(),
                "personal_2".into()
            )),
            "{strategy}: {v}"
        );
        assert_eq!(
            fs::read(app(&b).join("profiles/work/auth.json")).unwrap(),
            local_work
        );
        assert!(app(&b).join("profiles/work_2/auth.json").exists());
        assert_eq!(v["source_current_target"], "work_2");
    }
}

// 9
#[test]
fn dry_run_writes_nothing_and_prints_no_secrets() {
    let (_a, bundle) = bundle_of("dry");
    let b = machine_b_with_conflict("2026-10-01T00:00:00Z");
    let before = tree(&b);
    for strategy in ["skip", "overwrite", "merge"] {
        let out = import(&b, &bundle, &["--dry-run", "--strategy", strategy], PASS);
        assert!(out.status.success(), "{}", text(&out));
        let all = text(&out);
        assert!(all.contains("profiles/personal/auth.json"), "{all}");
        assert!(all.contains("machine-specific"), "{all}");
        for secret in [
            "refresh-work-a",
            "access-work-a",
            "sk-provider-secret",
            "proxy-secret-pw",
        ] {
            assert!(!all.contains(secret), "{secret} leaked: {all}");
        }
    }
    assert_eq!(before, tree(&b));
}

// 10
#[test]
fn failure_mid_apply_rolls_back() {
    let (_a, bundle) = bundle_of("rollback");
    let b = machine_b_with_conflict("2026-10-01T00:00:00Z");
    write(app(&b).join("config.toml"), "[cache]\nttl = 5\n");
    let before = tree(&b);
    let mut cmd = command(
        &b,
        &[
            "--json",
            "import",
            bundle.to_str().unwrap(),
            "--passphrase-stdin",
            "--strategy",
            "overwrite",
            "--yes",
        ],
    );
    cmd.env("CODEX_SWITCH_TEST_IMPORT_FAIL_AT", "3");
    let out = run_with_stdin(cmd, &format!("{PASS}\n"));
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("rolled back"), "{}", text(&out));
    let after: BTreeMap<_, _> = tree(&b)
        .into_iter()
        .filter(|(k, _)| !k.contains("import-backups"))
        .collect();
    assert_eq!(before, after);
    let backups = app(&b).join("import-backups");
    let dir = fs::read_dir(&backups)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(read_json(&dir.join("journal.json"))["state"], "rolled-back");

    // A later import is not blocked by a rolled-back journal.
    let (out, _) = json_import(&b, &bundle, &["--dry-run"], PASS);
    assert!(out.status.success(), "{}", text(&out));
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export-v1.csbundle")
}

// 11
#[test]
fn old_and_future_bundle_versions() {
    let b = temp_home("b");
    let (out, v) = json_import(&b, &fixture(), &[], "fixture-passphrase");
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(action_of(&v, "profiles/fixture/auth.json"), "create");
    assert!(app(&b).join("profiles/fixture/auth.json").exists());

    let dir = temp_home("future");
    let future = dir.join("future.csbundle");
    fs::copy(fixture(), &future).unwrap();
    let future = edit_bundle(&future, |v| v["schema_version"] = json!(2));
    // No stdin at all: the version error must come before any passphrase prompt.
    let out = run(&b, &["--json", "import", future.to_str().unwrap()], None);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["code"], "unsupported_bundle_version", "{v}");
    assert!(v["error"].as_str().unwrap().contains("self-update"));

    // Without the magic the file is not a bundle, so it takes the legacy path
    // where bundle-only flags are a usage error.
    let not_bundle = edit_bundle(&future, |v| v["format"] = json!("something-else"));
    let out = run(
        &b,
        &["import", not_bundle.to_str().unwrap(), "--dry-run"],
        None,
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

// 12
#[test]
fn plaintext_requires_confirmation() {
    let a = machine_a();
    let dest = temp_home("pt").join("plain.csbundle");
    let out = run(&a, &["export", dest.to_str().unwrap(), "--plaintext"], None);
    assert!(!out.status.success());
    assert!(!dest.exists());

    let out = run(
        &a,
        &["export", dest.to_str().unwrap(), "--plaintext", "--yes"],
        None,
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("NOT encrypted"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let v = read_json(&dest);
    assert!(v["encryption"].is_null());

    let b = temp_home("b");
    let out = run(&b, &["import", dest.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("NOT encrypted"));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Delete"));
    assert!(app(&b).join("profiles/work/auth.json").exists());
}

// 13
#[test]
fn encrypted_bundle_contains_no_secret_bytes() {
    let (_a, bundle) = bundle_of("bytes");
    let raw = fs::read(&bundle).unwrap();
    let v = read_json(&bundle);
    let ct = base64::engine::general_purpose::STANDARD
        .decode(v["ciphertext"].as_str().unwrap())
        .unwrap();
    for secret in [
        "refresh-work-a",
        "access-work-a",
        "refresh-me-a",
        "sk-provider-secret",
        "proxy-secret-pw",
        "acct-work",
        "work@example.com",
    ] {
        let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        for hay in [&raw, &ct] {
            for needle in [secret.as_bytes(), b64.as_bytes()] {
                assert!(
                    !hay.windows(needle.len()).any(|w| w == needle),
                    "{secret} found in bundle bytes"
                );
            }
        }
    }
}

fn plaintext_bundle(entries: &[(&str, &str, &str)]) -> PathBuf {
    // Serialise in the payload struct's field order so the checksum matches.
    let entries_json: Vec<String> = entries
        .iter()
        .map(|(key, kind, text)| {
            format!(
                "{{\"key\":{},\"kind\":{},\"text\":{}}}",
                json!(key),
                json!(kind),
                json!(text)
            )
        })
        .collect();
    let payload_str = format!(
        "{{\"source_current\":null,\"entries\":[{}]}}",
        entries_json.join(",")
    );
    use sha2::Digest;
    let digest = hex::encode(sha2::Sha256::digest(payload_str.as_bytes()));
    let doc = format!(
        "{{\"format\":\"codex-switch-export\",\"schema_version\":1,\"app_version\":\"test\",\"platform\":{{\"os\":\"linux\",\"arch\":\"x86_64\"}},\"created_at\":\"2026-10-19T00:00:00Z\",\"encryption\":null,\"payload_sha256\":\"{digest}\",\"payload\":{payload_str}}}"
    );
    let path = temp_home("hand").join("hand.csbundle");
    fs::write(&path, doc).unwrap();
    path
}

// 14
#[test]
fn unsafe_keys_are_rejected_before_any_write() {
    let good = serde_json::to_string(&auth(
        "ok@example.com",
        "acct-ok",
        "ok",
        "2026-10-01T00:00:00Z",
    ))
    .unwrap();
    // Sanity: a well-formed handcrafted bundle imports.
    let b = temp_home("b");
    let ok = plaintext_bundle(&[("profiles/ok/auth.json", "profile_auth", &good)]);
    let out = run(&b, &["import", ok.to_str().unwrap()], None);
    assert!(out.status.success(), "{}", text(&out));

    for bad in [
        "../x",
        "/etc/x",
        "profiles/a/../../x",
        "C:\\x",
        "profiles/a/other.json",
    ] {
        let b = temp_home("b");
        let path = plaintext_bundle(&[
            ("profiles/ok/auth.json", "profile_auth", &good),
            (bad, "profile_auth", &good),
        ]);
        let before = tree(&b);
        let out = run(&b, &["--json", "import", path.to_str().unwrap()], None);
        assert!(!out.status.success(), "{bad}: {}", text(&out));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["code"], "invalid_bundle", "{bad}: {v}");
        assert_eq!(before, tree(&b), "{bad} wrote something");
    }
}

// 15
#[test]
fn export_does_not_clobber_without_force() {
    let a = machine_a();
    let dest = temp_home("nc").join("exists.csbundle");
    fs::write(&dest, "keep me").unwrap();
    let out = run(
        &a,
        &["export", dest.to_str().unwrap(), "--passphrase-stdin"],
        Some("pw\n"),
    );
    assert!(!out.status.success());
    assert_eq!(fs::read(&dest).unwrap(), b"keep me");
    let out = run(
        &a,
        &[
            "export",
            dest.to_str().unwrap(),
            "--passphrase-stdin",
            "--force",
        ],
        Some("pw\n"),
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(read_json(&dest)["format"], "codex-switch-export");
}

// 16
#[test]
fn legacy_import_paths_are_unchanged() {
    let (_a, bundle) = bundle_of("legacy");
    let b = temp_home("b");
    // A directory scan only collects *.json, so a bundle alone is "no JSON files".
    let dir = temp_home("scan");
    fs::copy(&bundle, dir.join("backup.csbundle")).unwrap();
    let out = run(&b, &["import", dir.to_str().unwrap()], None);
    assert!(!out.status.success());
    assert!(text(&out).contains("no JSON files"), "{}", text(&out));
    // Bundle-only flags on a legacy auth.json path are a usage error.
    let auth_path = dir.join("auth.json");
    fs::write(
        &auth_path,
        serde_json::to_vec(&auth(
            "l@example.com",
            "acct-l",
            "l",
            "2026-10-01T00:00:00Z",
        ))
        .unwrap(),
    )
    .unwrap();
    for flag in [
        "--dry-run",
        "--strategy=merge",
        "--yes",
        "--activate",
        "--passphrase-stdin",
    ] {
        let out = run(&b, &["import", auth_path.to_str().unwrap(), flag], None);
        assert_eq!(out.status.code(), Some(2), "{flag}: {}", text(&out));
    }
    // An alias is a usage error for bundles.
    let out = run(&b, &["import", bundle.to_str().unwrap(), "alias"], None);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

// 17
#[test]
fn json_output_is_exactly_one_value() {
    let a = machine_a();
    let dest = temp_home("json").join("j.csbundle");
    let out = run(
        &a,
        &[
            "--json",
            "export",
            dest.to_str().unwrap(),
            "--passphrase-stdin",
        ],
        Some("pw\n"),
    );
    assert!(out.status.success(), "{}", text(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["encrypted"], true);
    assert_eq!(v["source_current"], "work");

    let b = temp_home("b");
    for extra in [&["--dry-run"][..], &[][..]] {
        let (out, v) = json_import(&b, &dest, extra, "pw");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(v["ok"], true);
        assert_eq!(
            serde_json::Deserializer::from_slice(&out.stdout)
                .into_iter::<Value>()
                .count(),
            1
        );
    }
}

#[test]
fn activate_switches_to_source_current() {
    let (_a, bundle) = bundle_of("activate");
    let b = temp_home("b");
    let (out, v) = json_import(&b, &bundle, &["--activate"], PASS);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(v["activated"], "work");
    assert_eq!(
        fs::read_to_string(app(&b).join("current")).unwrap().trim(),
        "work"
    );
    assert!(b.join(".codex/auth.json").exists());
}

#[test]
fn second_import_is_unchanged() {
    let (_a, bundle) = bundle_of("again");
    let b = temp_home("b");
    assert!(import(&b, &bundle, &[], PASS).status.success());
    let (out, v) = json_import(&b, &bundle, &[], PASS);
    assert!(out.status.success(), "{}", text(&out));
    for (key, action, _) in items(&v) {
        assert_eq!(action, "unchanged", "{key}");
    }
    assert!(v["backup_dir"].is_null());
}
