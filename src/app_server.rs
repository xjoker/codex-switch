//! Restart of the shared Codex app-server daemon after a credential change.
//!
//! Codex CLI 0.157 and newer attaches interactive sessions to a managed local
//! app-server daemon. That daemon loads `auth.json` once and re-reads it only
//! for the account it already holds, so once codex-switch replaces the file
//! with another account every new Codex session keeps using the previous one
//! until the daemon is restarted. `codex exec` and `codex --no-daemon` run in
//! process and read the file at startup, so they are not affected.

use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Upper bound for each `codex app-server daemon …` call. A switch must not
/// hang on a daemon that stops answering; the restart is then reported as
/// failed with the manual command.
const DAEMON_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// What happened to the managed app-server daemon after the live `auth.json`
/// changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRestart {
    /// No managed daemon is running, or this Codex has no daemon; nothing to do.
    NotRunning,
    /// The live `auth.json` is byte-identical to the snapshot taken before the
    /// change, so the daemon already holds it.
    Unchanged,
    Restarted,
    /// The daemon is running but `use.restart_app_server = false` keeps it
    /// untouched, so it still holds the previous account.
    Disabled,
    /// The daemon is running but did not restart. The live `auth.json` is
    /// already switched, so this is a warning rather than a failed switch.
    Failed(String),
}

impl DaemonRestart {
    /// One line for the user once `alias` is live, or `None` when nothing happened.
    pub fn message(&self, alias: &str) -> Option<String> {
        match self {
            Self::NotRunning | Self::Unchanged => None,
            Self::Restarted => Some(format!(
                "Restarted the Codex app-server daemon; new and reconnecting Codex sessions use '{alias}'."
            )),
            Self::Disabled => Some(format!(
                "Note: the running Codex app-server daemon still holds the previous account (use.restart_app_server = false). Run `codex app-server daemon restart` so Codex sessions use '{alias}'."
            )),
            Self::Failed(detail) => Some(format!(
                "Warning: the Codex app-server daemon still holds the previous account ({detail}). Run `codex app-server daemon restart` so Codex sessions use '{alias}'."
            )),
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// Content hash of the live `auth.json`, taken before a credential change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAuthSnapshot(Option<String>);

pub fn snapshot_live_auth() -> LiveAuthSnapshot {
    LiveAuthSnapshot(
        crate::auth::codex_auth_path()
            .ok()
            .and_then(|path| crate::auth::sha256_file(&path)),
    )
}

/// Restart the managed app-server daemon when one is running and the live
/// `auth.json` no longer matches `before`. Re-selecting the profile that is
/// already live leaves the file identical and needs no restart, which keeps a
/// scheduled `use` from interrupting Codex sessions on every run.
pub fn restart_daemon_if_live_auth_changed(before: &LiveAuthSnapshot) -> DaemonRestart {
    if live_auth_unchanged(before, &snapshot_live_auth()) {
        return DaemonRestart::Unchanged;
    }
    restart_daemon_if_running(crate::config::get().use_cfg.restart_app_server)
}

/// A missing or unreadable file before the change gives no evidence of what
/// the daemon holds, so that counts as changed.
fn live_auth_unchanged(before: &LiveAuthSnapshot, after: &LiveAuthSnapshot) -> bool {
    before.0.is_some() && before == after
}

/// Restart the managed app-server daemon when one is running, so the next
/// Codex session reads the switched `auth.json`. A daemon that is not running
/// is left alone: Codex starts one on demand and it then loads the current file.
fn restart_daemon_if_running(allow_restart: bool) -> DaemonRestart {
    let Some(codex) = crate::launch::command_on_path("codex") else {
        tracing::debug!("codex not found in PATH; skipping app-server daemon restart");
        return DaemonRestart::NotRunning;
    };
    restart_with(allow_restart, |args| {
        let mut command = Command::new(&codex);
        command.args(args);
        output_with_timeout(command, DAEMON_COMMAND_TIMEOUT)
    })
}

/// `Command::output` with a deadline: the child is killed once `timeout`
/// passes and the call returns `TimedOut`.
fn output_with_timeout(mut command: Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes on their own threads so a chatty child cannot block
    // on a full pipe while it is being waited on.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("codex did not answer within {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn restart_with<F>(allow_restart: bool, mut run_codex: F) -> DaemonRestart
where
    F: FnMut(&[&str]) -> io::Result<Output>,
{
    match run_codex(&["app-server", "daemon", "version"]) {
        Ok(output) if daemon_is_running(&output) => {}
        Ok(output) => {
            tracing::debug!(
                "no running Codex app-server daemon to restart: {}",
                failure_detail(&output)
            );
            return DaemonRestart::NotRunning;
        }
        Err(err) => {
            tracing::debug!("could not query the Codex app-server daemon: {err}");
            return DaemonRestart::NotRunning;
        }
    }
    if !allow_restart {
        return DaemonRestart::Disabled;
    }

    match run_codex(&["app-server", "daemon", "restart"]) {
        Ok(output) if output.status.success() => DaemonRestart::Restarted,
        Ok(output) => DaemonRestart::Failed(failure_detail(&output)),
        Err(err) => DaemonRestart::Failed(err.to_string()),
    }
}

/// `codex app-server daemon version` prints `{"status":"running",…}` for a
/// live daemon. A stopped daemon fails to connect, and a Codex that predates
/// the daemon rejects the subcommand; neither must be restarted, because
/// `restart` would start a daemon nobody asked for.
fn daemon_is_running(output: &Output) -> bool {
    output.status.success()
        && serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .is_some_and(|version| version["status"] == "running")
}

fn failure_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => line.to_string(),
        None => format!("codex exited with {}", output.status),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonRestart, LiveAuthSnapshot, live_auth_unchanged, output_with_timeout, restart_with,
    };
    use std::cell::RefCell;
    use std::io;
    use std::process::{Command, ExitStatus, Output};
    use std::time::{Duration, Instant};

    fn exit_status(code: i32) -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code as u32)
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    const RUNNING: &str = r#"{"status":"running","backend":"pid","cliVersion":"0.158.0","appServerVersion":"0.159.0"}"#;

    /// Records every codex invocation and answers each from a script.
    struct FakeCodex {
        calls: RefCell<Vec<Vec<String>>>,
        answers: RefCell<Vec<io::Result<Output>>>,
    }

    impl FakeCodex {
        fn new(answers: Vec<io::Result<Output>>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                answers: RefCell::new(answers.into_iter().rev().collect()),
            }
        }

        fn run(&self, args: &[&str]) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|arg| arg.to_string()).collect());
            self.answers
                .borrow_mut()
                .pop()
                .expect("more codex invocations than scripted answers")
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    const VERSION_ARGS: [&str; 3] = ["app-server", "daemon", "version"];
    const RESTART_ARGS: [&str; 3] = ["app-server", "daemon", "restart"];

    #[test]
    fn running_daemon_is_restarted() {
        let codex = FakeCodex::new(vec![
            Ok(output(0, RUNNING, "")),
            Ok(output(0, r#"{"status":"restarted"}"#, "")),
        ]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::Restarted
        );
        assert_eq!(
            codex.calls(),
            vec![VERSION_ARGS.to_vec(), RESTART_ARGS.to_vec()]
        );
    }

    #[test]
    fn stopped_daemon_is_left_alone() {
        let codex = FakeCodex::new(vec![Ok(output(
            1,
            "",
            "Error: failed to connect to /home/u/.codex/app-server-control/app-server-control.sock",
        ))]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
        assert_eq!(codex.calls(), vec![VERSION_ARGS.to_vec()]);
    }

    #[test]
    fn codex_without_daemon_subcommand_is_left_alone() {
        let codex = FakeCodex::new(vec![Ok(output(
            2,
            "",
            "error: unrecognized subcommand 'daemon'",
        ))]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
        assert_eq!(codex.calls(), vec![VERSION_ARGS.to_vec()]);
    }

    #[test]
    fn version_that_does_not_report_running_is_left_alone() {
        let codex = FakeCodex::new(vec![Ok(output(0, r#"{"status":"notRunning"}"#, ""))]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
        let codex = FakeCodex::new(vec![Ok(output(0, "not json", ""))]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
    }

    #[test]
    fn unspawnable_codex_is_left_alone() {
        let codex = FakeCodex::new(vec![Err(io::Error::from(io::ErrorKind::NotFound))]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
        assert_eq!(codex.calls(), vec![VERSION_ARGS.to_vec()]);
    }

    #[test]
    fn failed_restart_reports_the_first_stderr_line() {
        let codex = FakeCodex::new(vec![
            Ok(output(0, RUNNING, "")),
            Ok(output(
                1,
                "",
                "\nError: app server is running but is not managed by codex app-server daemon\n\nCaused by:\n    something\n",
            )),
        ]);
        assert_eq!(
            restart_with(true, |args| codex.run(args)),
            DaemonRestart::Failed(
                "Error: app server is running but is not managed by codex app-server daemon".into()
            )
        );
        assert_eq!(
            codex.calls(),
            vec![VERSION_ARGS.to_vec(), RESTART_ARGS.to_vec()]
        );
    }

    #[test]
    fn failed_restart_without_stderr_reports_the_exit_status() {
        let codex = FakeCodex::new(vec![Ok(output(0, RUNNING, "")), Ok(output(3, "", "  \n"))]);
        let DaemonRestart::Failed(detail) = restart_with(true, |args| codex.run(args)) else {
            panic!("a failed restart must be reported");
        };
        assert!(detail.contains('3'), "{detail}");
    }

    #[test]
    fn restart_spawn_error_is_a_failure() {
        let codex = FakeCodex::new(vec![
            Ok(output(0, RUNNING, "")),
            Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        ]);
        assert!(restart_with(true, |args| codex.run(args)).is_failure());
    }

    #[test]
    fn disabled_restart_only_probes_the_daemon() {
        let codex = FakeCodex::new(vec![Ok(output(0, RUNNING, ""))]);
        assert_eq!(
            restart_with(false, |args| codex.run(args)),
            DaemonRestart::Disabled
        );
        assert_eq!(codex.calls(), vec![VERSION_ARGS.to_vec()]);
        let note = DaemonRestart::Disabled.message("work").unwrap();
        assert!(note.contains("codex app-server daemon restart"), "{note}");
        assert!(!DaemonRestart::Disabled.is_failure());
    }

    #[test]
    fn stopped_daemon_is_not_reported_when_restart_is_disabled() {
        let codex = FakeCodex::new(vec![Ok(output(1, "", "Error: failed to connect"))]);
        assert_eq!(
            restart_with(false, |args| codex.run(args)),
            DaemonRestart::NotRunning
        );
    }

    #[test]
    fn hung_codex_is_killed_at_the_deadline() {
        let command = if cfg!(windows) {
            let mut c = Command::new("powershell.exe");
            c.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        let started = Instant::now();
        let err = output_with_timeout(command, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn identical_live_auth_needs_no_restart() {
        let a = LiveAuthSnapshot(Some("a".into()));
        let b = LiveAuthSnapshot(Some("b".into()));
        let missing = LiveAuthSnapshot(None);
        assert!(live_auth_unchanged(&a, &a));
        assert!(!live_auth_unchanged(&a, &b));
        assert!(!live_auth_unchanged(&a, &missing));
        assert!(!live_auth_unchanged(&missing, &a));
        assert!(
            !live_auth_unchanged(&missing, &missing),
            "no file before the change says nothing about what the daemon holds"
        );
    }

    #[test]
    fn messages_name_the_live_alias() {
        assert_eq!(DaemonRestart::NotRunning.message("work"), None);
        assert_eq!(DaemonRestart::Unchanged.message("work"), None);
        let restarted = DaemonRestart::Restarted.message("work").unwrap();
        assert!(restarted.contains("'work'"), "{restarted}");
        assert!(restarted.starts_with("Restarted"), "{restarted}");
        let failed = DaemonRestart::Failed("boom".into())
            .message("work")
            .unwrap();
        assert!(failed.contains("boom"), "{failed}");
        assert!(
            failed.contains("codex app-server daemon restart"),
            "{failed}"
        );
        assert!(failed.contains("'work'"), "{failed}");
    }
}
