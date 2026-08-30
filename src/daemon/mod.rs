pub mod codex_process;
pub mod loop_runner;
pub mod notify;
pub mod pidfile;
pub mod service;
pub mod state;

use crate::cli::DaemonCommand;
use crate::output::{print_json, user_println};
use anyhow::Result;

pub async fn dispatch(cmd: DaemonCommand, json: bool) -> Result<()> {
    match cmd {
        DaemonCommand::Start { foreground } => start(foreground).await,
        DaemonCommand::Stop => stop(),
        DaemonCommand::Status => status(json),
        DaemonCommand::Install => service::install(),
        DaemonCommand::Uninstall => uninstall(),
    }
}

fn uninstall() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Task Scheduler's `/End` is a forced stop. If its daemon is live,
        // give the process the same generation-bound graceful request used by
        // `daemon stop` before removing the task.
        let pid = pidfile::read_pidfile();
        let alive = pid.is_some_and(pidfile::process_alive);
        if let WindowsStopGate::Graceful = windows_stop_gate(pid, alive, pidfile::cleanup_pidfile)?
        {
            stop_detached()?;
        }
        // Re-check immediately before `service::uninstall()` reaches
        // Task Scheduler's `/End`. A transient false from the wait loop cannot
        // authorize a force-stop while the daemon still owns its PID lock.
        let final_pid = pidfile::read_pidfile();
        let final_alive = final_pid.is_some_and(pidfile::process_alive);
        if let WindowsStopGate::Graceful =
            windows_stop_gate(final_pid, final_alive, pidfile::cleanup_pidfile)?
        {
            anyhow::bail!(
                "Daemon is still running after the graceful stop request; refusing to \
                 force-terminate it during uninstall"
            );
        }
    }
    service::uninstall()
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Eq, PartialEq)]
enum WindowsStopGate {
    Graceful,
    SchedulerSafe,
}

/// Decide whether Task Scheduler may use `/End`.
///
/// Both `process_alive == false` and a missing parsed PID are ambiguous on
/// Windows because tasklist, path lookup, file reads, parsing, and lock probes
/// can fail. Removing the PID file requires taking its exclusive lock, so a
/// diagnostic failure while the daemon still owns that lock returns an error
/// and fails closed instead of authorizing a forced stop. A genuinely absent
/// file makes cleanup a harmless no-op.
#[cfg(any(target_os = "windows", test))]
fn windows_stop_gate(
    pid: Option<u32>,
    process_alive: bool,
    cleanup_stale_pidfile: impl FnOnce() -> Result<()>,
) -> Result<WindowsStopGate> {
    match pid {
        Some(_) if process_alive => Ok(WindowsStopGate::Graceful),
        Some(_) => {
            cleanup_stale_pidfile()?;
            Ok(WindowsStopGate::SchedulerSafe)
        }
        None => {
            cleanup_stale_pidfile()?;
            Ok(WindowsStopGate::SchedulerSafe)
        }
    }
}

async fn start(foreground: bool) -> Result<()> {
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = foreground;
        anyhow::bail!("The background daemon is not supported on this platform.");
    }
    #[cfg(any(unix, target_os = "windows"))]
    {
        if pidfile::is_daemon_running() {
            anyhow::bail!(
                "Daemon is already running (PID {})",
                pidfile::read_pidfile().unwrap_or(0)
            );
        }
        // Clean up stale PID file before starting
        pidfile::cleanup_pidfile()?;
        if foreground {
            return run_foreground().await;
        }
        if service::is_installed() {
            return service::start_installed();
        }
        start_detached()
    }
}

async fn run_foreground() -> Result<()> {
    pidfile::write_pidfile_exclusive()?;
    // RAII guard ensures PID file is cleaned up even on panic
    let _guard = pidfile::PidGuard;
    tracing::info!("codex-switch daemon started (PID {})", std::process::id());
    loop_runner::run_daemon_loop().await
}

fn start_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let pid = await_daemon_ready(&mut child, STARTUP_TIMEOUT)?;
    user_println(&format!("Daemon started (PID {pid})"));
    Ok(())
}

/// How long a freshly spawned daemon gets to publish its PID file.
///
/// Generous on purpose: the wait below returns the moment the file appears, so
/// the only thing a large value costs is how long a genuinely broken start
/// takes to be reported. A tight bound, on the other hand, turns a cold binary
/// on a slow disk — a fresh self-update, an on-access virus scan, a loaded CI
/// runner — into a spurious "start failed".
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits for the daemon to write its PID file, which signals it reached the
/// event loop. Polling the actual readiness signal is more reliable than a
/// fixed sleep on slow disks / CI / containers.
///
/// A child that never gets there is killed rather than left running. It is
/// spawned detached, so abandoning it would report a failed start while an
/// initializing daemon is still on its way — leaving the user with a process
/// they were told does not exist, and a retry that refuses with "already
/// running". Nothing is lost by killing it: not having written the PID file is
/// exactly what says it has not begun touching credentials yet.
fn await_daemon_ready(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Result<u32> {
    let pid = child.id();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // Did the child exit before initializing?
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "Daemon process (PID {pid}) exited immediately ({status}); check logs for details"
            );
        }
        if pidfile::read_pidfile() == Some(pid) {
            return Ok(pid);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "Daemon (PID {pid}) did not initialize within {}s (no PID file written) and was \
                 stopped; check logs",
                timeout.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn stop() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // A scheduled task runs the same foreground daemon. Ask that process
        // to unwind first; `/End` would terminate it during credential writes.
        let pid = pidfile::read_pidfile();
        let alive = pid.is_some_and(pidfile::process_alive);
        if let WindowsStopGate::Graceful = windows_stop_gate(pid, alive, pidfile::cleanup_pidfile)?
        {
            return stop_detached();
        }
        // An older or still-starting scheduled task may not have a trusted
        // pidfile. There is no generation-bound process to signal, so Task
        // Scheduler is the only remaining stop authority.
        if service::is_installed() {
            service::stop_installed()?;
            let _ = pidfile::cleanup_pidfile();
            return Ok(());
        }
        stop_detached()
    }

    #[cfg(not(target_os = "windows"))]
    {
        if service::is_installed() {
            service::stop_installed()?;
            wait_until_stopped_or_kill(pidfile::read_pidfile())?;
            let _ = pidfile::cleanup_pidfile();
            return Ok(());
        }

        stop_detached()
    }
}

fn stop_detached() -> Result<()> {
    let pid = pidfile::read_pidfile()
        .ok_or_else(|| anyhow::anyhow!("No daemon PID file found; daemon may not be running"))?;
    if !pidfile::process_alive(pid) {
        pidfile::cleanup_pidfile()?;
        user_println("Daemon was not running (stale PID file cleaned up)");
        return Ok(());
    }
    pidfile::send_sigterm(pid)?;
    #[cfg(target_os = "windows")]
    {
        wait_until_stopped(Some(pid)).map_err(|err| {
            anyhow::anyhow!(
                "{err}. The daemon may still be finishing an in-flight credential rotation; \
                 refusing to force-terminate it. Retry `codex-switch daemon stop` shortly."
            )
        })?;
        // `process_alive` can return false when tasklist fails. Successfully
        // taking and deleting the PID file is the authoritative completion
        // proof; a live daemon's held lock makes this fail closed.
        pidfile::cleanup_pidfile()?;
    }
    user_println(&format!("Sent stop signal to daemon (PID {pid})"));
    Ok(())
}

fn wait_until_stopped_or_kill(pid: Option<u32>) -> Result<()> {
    match wait_until_stopped(pid) {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::warn!(
                "Daemon still running after service stop, falling back to PID stop: {err}"
            );
            stop_detached()?;
            wait_until_stopped(pid)
        }
    }
}

fn wait_until_stopped(pid: Option<u32>) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let running = match pid.or_else(pidfile::read_pidfile) {
            Some(pid) => pidfile::process_alive(pid),
            None => false,
        };
        if !running {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Daemon did not stop within 10s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub struct SelfUpdateDaemonRestart {
    pid: Option<u32>,
    service_installed: bool,
    stopped: bool,
}

impl SelfUpdateDaemonRestart {
    pub fn capture() -> Self {
        let pid = pidfile::read_pidfile().filter(|pid| pidfile::process_alive(*pid));
        Self {
            pid,
            service_installed: service::is_installed(),
            stopped: false,
        }
    }

    pub fn is_needed(&self) -> bool {
        self.pid.is_some()
    }

    pub fn stop_before_update(&mut self) -> Result<()> {
        if !self.is_needed() || self.stopped {
            return Ok(());
        }

        user_println("Stopping daemon before self-update...");
        if self.service_installed && !cfg!(target_os = "windows") {
            service::stop_installed()?;
            wait_until_stopped_or_kill(self.pid)?;
            let _ = pidfile::cleanup_pidfile();
        } else {
            stop_detached()?;
            wait_until_stopped(self.pid)?;
        }
        self.stopped = true;
        Ok(())
    }

    pub fn restart_after_update(&mut self) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }

        user_println("Restarting daemon after self-update...");
        if self.service_installed {
            service::start_installed()?;
        } else {
            start_detached()?;
        }
        self.stopped = false;
        Ok(())
    }
}

fn status(json: bool) -> Result<()> {
    let pidfile = pidfile::pidfile_path()?;
    let pid = pidfile::read_pidfile();
    let running = pid.is_some_and(pidfile::process_alive);
    let state = match (pid, running) {
        (Some(_), true) => "running",
        (Some(_), false) => "stale",
        (None, _) => "stopped",
    };

    // Loop-written snapshot; only meaningful while the daemon is running.
    let snapshot = if running { state::read() } else { None };

    if json {
        let cfg = crate::config::get();
        print_json(&serde_json::json!({
            "running": running,
            "state": state,
            "pid": pid,
            "pidfile": pidfile,
            "stale_pid_cleaned": state == "stale",
            "snapshot": snapshot,
            "platform": {
                "os": std::env::consts::OS,
                "daemon_start_supported": cfg!(any(unix, target_os = "windows")),
                "service_install_supported": cfg!(any(target_os = "macos", target_os = "linux", target_os = "windows")),
                "service_manager": service_manager_name(),
                "service_installed": service::is_installed(),
            },
            "config": {
                "poll_interval_secs": cfg.daemon.poll_interval_secs,
                "cache_refresh_interval_secs": cfg.daemon.cache_refresh_interval_secs,
                "auto_warmup": cfg.daemon.auto_warmup,
                "warmup_times": cfg.daemon.warmup_times,
                "timezone": cfg.daemon.timezone,
                "token_check_interval_secs": cfg.daemon.token_check_interval_secs,
                "switch_threshold": cfg.daemon.switch_threshold,
                "notify": cfg.daemon.notify,
                "log_level": cfg.daemon.log_level,
            }
        }));
        if state == "stale" {
            pidfile::cleanup_pidfile()?;
        }
        return Ok(());
    }

    #[cfg(any(unix, target_os = "windows"))]
    {
        match (pid, running) {
            (Some(pid), true) => {
                user_println(&format!("Daemon is running (PID {pid})"));
                if let Some(snap) = &snapshot {
                    if let Some(at) = snap.last_poll_at {
                        user_println(&format!("  Last poll: {}", format_unix(at)));
                    }
                    if let Some(sw) = &snap.last_switch {
                        user_println(&format!(
                            "  Last switch: '{}' -> '{}' at {} (score {:.0})",
                            sw.from,
                            sw.to,
                            format_unix(sw.at),
                            sw.score
                        ));
                    }
                    if let Some(p) = &snap.pending_switch {
                        user_println(&format!(
                            "  Pending switch to '{}' since {} (waiting for Codex session to end)",
                            p.to,
                            format_unix(p.since)
                        ));
                    }
                    if let Some(err) = &snap.last_error {
                        user_println(&format!(
                            "  Last error ({} consecutive): {err}",
                            snap.consecutive_failures
                        ));
                    }
                    // Repeated failures back polling off by up to sixteen
                    // intervals. Without this line the daemon reads as healthy
                    // while it is deliberately idle, so someone who has just
                    // fixed the cause has no way to tell how long the fix will
                    // take to show up — or that restarting would apply it now.
                    if let Some(until) = snap.backoff_until {
                        let remaining = until - crate::auth::now_unix_secs();
                        if remaining > 0 {
                            user_println(&format!(
                                "  Polling suspended for another {remaining}s (until {}) after \
                                 repeated failures; `daemon stop` then `daemon start` resumes it \
                                 immediately",
                                format_unix(until)
                            ));
                        }
                    }
                    if let Some(slot) = &snap.last_warmup_slot {
                        user_println(&format!("  Last warmup slot: {slot}"));
                    }
                }
                let cfg = crate::config::get();
                if cfg.daemon.warmup_times.is_empty() {
                    user_println(&format!(
                        "  auto_warmup={} (empty warmup_times: cache-refresh warmup when on)",
                        cfg.daemon.auto_warmup
                    ));
                } else {
                    user_println(&format!(
                        "  auto_warmup={} warmup_times=[{}]",
                        cfg.daemon.auto_warmup,
                        cfg.daemon.warmup_times.join(", ")
                    ));
                }
                user_println(&format!(
                    "  timezone={}",
                    crate::warmup_schedule::timezone_label(&cfg.daemon.timezone)
                ));
            }
            (Some(pid), false) => {
                user_println(&format!("Daemon is not running (stale PID {pid})"));
                pidfile::cleanup_pidfile()?;
            }
            (None, _) => {
                user_println("Daemon is not running");
            }
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        user_println(&format!(
            "Daemon is not supported on this platform ({})",
            std::env::consts::OS
        ));
    }
    Ok(())
}

#[cfg(any(unix, target_os = "windows"))]
fn format_unix(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn service_manager_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd-user"
    }
    #[cfg(target_os = "windows")]
    {
        "task-scheduler"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "unsupported"
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    /// A `daemon start` that reports failure must leave no daemon behind. The
    /// child is spawned detached, so abandoning it on timeout hands the user a
    /// process they were just told does not exist — and a second
    /// `daemon start` that then refuses with "already running".
    #[test]
    fn a_daemon_that_never_signals_readiness_is_killed_not_abandoned() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os("CODEX_SWITCH_HOME");
        // SAFETY: the process-wide env lock above is held for the whole test.
        unsafe { std::env::set_var("CODEX_SWITCH_HOME", home.path()) };

        // Stands in for a daemon that starts but never reaches the event loop:
        // it stays alive and writes no PID file into the empty home above.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn stand-in daemon");
        let pid = child.id();

        let err = super::await_daemon_ready(&mut child, Duration::from_millis(200))
            .expect_err("no PID file is ever written, so readiness cannot be reached");

        // SAFETY: same held lock.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }

        assert!(
            err.to_string().contains("did not initialize"),
            "unexpected error: {err}"
        );
        // `process_alive` is not the check to make here: it reads the PID file
        // first and so answers "no" for any PID once that file is absent,
        // which is exactly the state under test. Reaping the child is the
        // direct evidence — it can only have been killed and waited for.
        assert!(
            child.try_wait().expect("try_wait").is_some(),
            "the daemon reported as failed is still running as PID {pid}"
        );
    }
}

#[cfg(test)]
mod windows_stop_tests {
    use anyhow::anyhow;

    use super::{WindowsStopGate, windows_stop_gate};

    #[test]
    fn a_windows_process_probe_failure_cannot_authorize_a_forced_stop() {
        let err = windows_stop_gate(Some(4242), false, || {
            Err(anyhow!("PID file is still locked by the daemon"))
        })
        .expect_err("an inconclusive process probe must fail closed");

        assert!(err.to_string().contains("still locked"));
    }

    #[test]
    fn only_an_unlocked_stale_pidfile_authorizes_the_scheduler_stop() {
        assert_eq!(
            windows_stop_gate(Some(4242), false, || Ok(())).unwrap(),
            WindowsStopGate::SchedulerSafe
        );
        assert_eq!(
            windows_stop_gate(Some(4242), true, || {
                panic!("a live daemon must use its generation-bound request")
            })
            .unwrap(),
            WindowsStopGate::Graceful
        );
    }

    #[test]
    fn uninstall_rechecks_the_pid_lock_after_a_graceful_request() {
        assert_eq!(
            windows_stop_gate(Some(4242), true, || {
                panic!("the initial live process must receive a graceful request")
            })
            .unwrap(),
            WindowsStopGate::Graceful
        );

        let err = windows_stop_gate(Some(4242), false, || {
            Err(anyhow!(
                "PID lock is still held after a false stopped probe"
            ))
        })
        .expect_err("the final pre-/End gate must fail closed");
        assert!(err.to_string().contains("still held"));
    }

    #[test]
    fn an_unreadable_pidfile_cannot_be_treated_as_no_daemon() {
        let err = windows_stop_gate(None, false, || {
            Err(anyhow!(
                "PID file could not be read but its lock is still held"
            ))
        })
        .expect_err("an ambiguous missing PID identity must still verify the PID-file lock");

        assert!(err.to_string().contains("still held"));
    }
}
