#![cfg(unix)]

mod mock;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mock::scenarios;
use serde_json::{Value, json};

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    usage_url: String,
    token_url: String,
}

impl TestEnv {
    fn app_home(&self) -> PathBuf {
        self.home.join(".codex-switch")
    }

    fn current_file(&self) -> PathBuf {
        self.app_home().join("current")
    }

    fn live_auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn pidfile(&self) -> PathBuf {
        self.app_home().join("daemon.pid")
    }
}

fn make_id_token(email: &str, plan_type: &str, account_id: &str) -> String {
    let claims = json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": plan_type,
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
            "organizations": [],
        }
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("header.{payload}.signature")
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn setup_env(
    entries: &[(String, Vec<Value>)],
    current_alias: &str,
    usage_url: String,
    token_url: String,
) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let codex_home = tmp.path().join("codex-home");
    let profiles_dir = home.join(".codex-switch").join("profiles");

    std::fs::create_dir_all(&profiles_dir).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();

    for (token, responses) in entries {
        let alias = token.strip_prefix("tok_").unwrap_or(token);
        let plan_type = responses[0]["plan_type"].as_str().unwrap_or("plus");
        let auth_json = json!({
            "tokens": {
                "access_token": token,
                "refresh_token": format!("refresh_{token}"),
                "id_token": make_id_token(
                    &format!("{alias}@mock.test"),
                    plan_type,
                    &format!("acct_{alias}")
                ),
                "account_id": format!("acct_{alias}"),
            }
        });
        write_json(&profiles_dir.join(alias).join("auth.json"), &auth_json);

        if alias == current_alias {
            write_json(&codex_home.join("auth.json"), &auth_json);
        }
    }

    std::fs::write(home.join(".codex-switch").join("current"), current_alias).unwrap();
    std::fs::write(
        home.join(".codex-switch").join("config.toml"),
        r#"[use]
safety_margin_7d = 20
team_priority = true

[daemon]
poll_interval_secs = 1
switch_threshold = 50
token_check_interval_secs = 60
notify = false
log_level = "error"
"#,
    )
    .unwrap();

    TestEnv {
        _tmp: tmp,
        home,
        codex_home,
        usage_url,
        token_url,
    }
}

fn run_cmd(env: &TestEnv, args: &[&str]) -> Output {
    let bin = std::env::var("CARGO_BIN_EXE_codex-switch").unwrap();
    Command::new(bin)
        .args(args)
        .env("HOME", &env.home)
        .env("CODEX_HOME", &env.codex_home)
        .env("CS_USAGE_URL", &env.usage_url)
        .env("CS_TOKEN_URL", &env.token_url)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn wait_until<F>(timeout: Duration, label: &str, mut check: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("condition '{}' not met within {:?}", label, timeout);
}

fn read_live_access_token(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_start_switch_status_and_stop() {
    let entries = scenarios::gradual_exhaustion();
    let server = mock::MockServer::start(entries.clone()).await;
    let env = setup_env(
        &entries,
        "gradual_a",
        server.usage_url(),
        server.token_url(),
    );

    let status_before = run_cmd(&env, &["daemon", "status"]);
    assert!(status_before.status.success());
    assert_eq!(stdout(&status_before), "Daemon is not running");

    let start = run_cmd(&env, &["daemon", "start"]);
    assert!(
        start.status.success(),
        "start stderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(stdout(&start).starts_with("Daemon started (PID "));

    wait_until(Duration::from_secs(10), "pidfile created", || {
        env.pidfile().exists()
    });

    wait_until(Duration::from_secs(10), "daemon status=running", || {
        let out = run_cmd(&env, &["daemon", "status"]);
        out.status.success() && stdout(&out).starts_with("Daemon is running (PID ")
    });

    wait_until(
        Duration::from_secs(15),
        "daemon switches to gradual_b",
        || {
            std::fs::read_to_string(env.current_file())
                .map(|s| s.trim() == "gradual_b")
                .unwrap_or(false)
                && read_live_access_token(&env.live_auth_path()).as_deref()
                    == Some("tok_gradual_b")
        },
    );

    let stop = run_cmd(&env, &["daemon", "stop"]);
    assert!(
        stop.status.success(),
        "stop stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stdout(&stop).starts_with("Sent stop signal to daemon (PID "));

    wait_until(
        Duration::from_secs(15),
        "daemon stopped and status=not running",
        || {
            let out = run_cmd(&env, &["daemon", "status"]);
            out.status.success() && stdout(&out) == "Daemon is not running"
        },
    );

    assert!(
        !env.pidfile().exists(),
        "pidfile should be removed after stop"
    );
    assert_eq!(
        std::fs::read_to_string(env.current_file()).unwrap().trim(),
        "gradual_b"
    );
    assert_eq!(
        read_live_access_token(&env.live_auth_path()).as_deref(),
        Some("tok_gradual_b")
    );

    server.shutdown();
}
