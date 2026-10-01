//! Authentication preflight must reject incompatible policy before touching
//! credentials, choosing an account or beginning an OAuth login.

use std::fs;
use std::process::{Command, Output};

struct Homes {
    root: tempfile::TempDir,
}

impl Homes {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("codex")).unwrap();
        fs::create_dir_all(root.path().join("switch/profiles/known")).unwrap();
        fs::write(root.path().join("codex/auth.json"), "live sentinel").unwrap();
        fs::write(root.path().join("switch/current"), "known").unwrap();
        fs::write(
            root.path().join("switch/profiles/known/auth.json"),
            "saved sentinel",
        )
        .unwrap();
        Self { root }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codex-switch"));
        command
            .args(["--json", "--color", "never"])
            .args(args)
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("CODEX_HOME", self.root.path().join("codex"))
            .env("CODEX_SWITCH_HOME", self.root.path().join("switch"))
            .env_remove("OPENAI_FEDERATION_RULE_ID")
            .env_remove("OPENAI_IDENTITY_TOKEN_FILE")
            .env_remove("OPENAI_WORKLOAD_IDENTITY_CONTEXT")
            .env_remove("RUST_LOG");
        command
    }

    fn assert_unchanged(&self) {
        for (path, expected) in [
            ("codex/auth.json", "live sentinel"),
            ("switch/current", "known"),
            ("switch/profiles/known/auth.json", "saved sentinel"),
        ] {
            assert_eq!(
                fs::read_to_string(self.root.path().join(path)).unwrap(),
                expected,
                "{path} changed before preflight rejection"
            );
        }
        assert!(!self.root.path().join("switch/provider-runs").exists());
    }
}

fn assert_single_json_error(output: &Output, expected: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json.to_string().contains(expected), "{json}");
}

#[test]
fn federation_presence_blocks_explicit_and_automatic_switch_before_file_changes() {
    for variable in ["OPENAI_FEDERATION_RULE_ID", "OPENAI_IDENTITY_TOKEN_FILE"] {
        for value in ["", "test-value"] {
            for args in [&["use", "known"][..], &["use"][..]] {
                let homes = Homes::new();
                let output = homes.command(args).env(variable, value).output().unwrap();
                assert_single_json_error(&output, variable);
                homes.assert_unchanged();
            }
        }
    }
}

#[test]
fn federation_preflight_precedes_import_file_collection_and_login() {
    for args in [
        &["import", "missing-input.json"][..],
        &["login", "known"][..],
        &["login", "--device"][..],
    ] {
        let homes = Homes::new();
        let output = homes
            .command(args)
            .env("OPENAI_FEDERATION_RULE_ID", "test-rule")
            .output()
            .unwrap();
        assert_single_json_error(&output, "OPENAI_FEDERATION_RULE_ID");
        homes.assert_unchanged();
    }
}

#[cfg(windows)]
#[test]
fn managed_store_default_takes_precedence_over_user_configuration() {
    let homes = Homes::new();
    fs::write(
        homes.root.path().join("codex/managed_config.toml"),
        "cli_auth_credentials_store = 'keyring'\n",
    )
    .unwrap();
    fs::write(
        homes.root.path().join("codex/config.toml"),
        "cli_auth_credentials_store = 'file'\n",
    )
    .unwrap();
    let output = homes.command(&["use", "known"]).output().unwrap();
    assert_single_json_error(&output, "keyring");
    homes.assert_unchanged();
}

#[test]
fn invalid_auth_setting_is_reported_before_login_or_credential_changes() {
    let homes = Homes::new();
    fs::write(
        homes.root.path().join("codex/config.toml"),
        "forced_login_method = 42\n",
    )
    .unwrap();
    let output = homes.command(&["login", "--device"]).output().unwrap();
    assert_single_json_error(&output, "forced_login_method");
    homes.assert_unchanged();
}

#[test]
fn custom_chatgpt_base_url_does_not_block_local_use_or_import() {
    // A proxy/mirror endpoint only matters to calls codex-switch makes to the
    // ChatGPT backend; switching and importing are purely local.
    for args in [&["use", "known"][..], &["import", "missing-input.json"][..]] {
        let homes = Homes::new();
        fs::write(
            homes.root.path().join("codex/config.toml"),
            "chatgpt_base_url = 'https://mirror.example/backend-api'
",
        )
        .unwrap();
        let output = homes.command(args).output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(
            !text.contains("chatgpt_base_url"),
            "{args:?} was refused by the endpoint policy: {text}"
        );
    }
}
