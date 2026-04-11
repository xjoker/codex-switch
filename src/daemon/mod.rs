pub mod loop_runner;
pub mod notify;
pub mod pidfile;
pub mod service;

use anyhow::Result;
use crate::cli::DaemonCommand;
use crate::output::user_println;

pub async fn dispatch(cmd: DaemonCommand) -> Result<()> {
    match cmd {
        DaemonCommand::Start { foreground } => {
            if pidfile::is_daemon_running() {
                anyhow::bail!(
                    "Daemon is already running (PID {})",
                    pidfile::read_pidfile().unwrap_or(0)
                );
            }
            if foreground {
                run_foreground().await
            } else {
                start_detached()
            }
        }
        DaemonCommand::Stop => stop(),
        DaemonCommand::Status => status(),
        DaemonCommand::Install => service::install(),
        DaemonCommand::Uninstall => service::uninstall(),
    }
}

async fn run_foreground() -> Result<()> {
    pidfile::write_pidfile()?;
    tracing::info!("codex-switch daemon started (PID {})", std::process::id());
    let result = loop_runner::run_daemon_loop().await;
    pidfile::cleanup_pidfile()?;
    result
}

fn start_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    user_println(&format!("Daemon started (PID {})", child.id()));
    Ok(())
}

fn stop() -> Result<()> {
    let pid = pidfile::read_pidfile()
        .ok_or_else(|| anyhow::anyhow!("No daemon PID file found; daemon may not be running"))?;
    if !pidfile::process_alive(pid) {
        pidfile::cleanup_pidfile()?;
        user_println("Daemon was not running (stale PID file cleaned up)");
        return Ok(());
    }
    #[cfg(unix)]
    {
        // Send SIGTERM via command (no libc dependency needed)
        let status = std::process::Command::new("kill")
            .args([&pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(e) = status {
            anyhow::bail!("Failed to send stop signal to PID {pid}: {e}");
        }
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("Stopping daemon is only supported on Unix; use Task Manager on Windows");
    }
    user_println(&format!("Sent stop signal to daemon (PID {pid})"));
    Ok(())
}

fn status() -> Result<()> {
    match pidfile::read_pidfile() {
        Some(pid) if pidfile::process_alive(pid) => {
            user_println(&format!("Daemon is running (PID {pid})"));
        }
        Some(pid) => {
            user_println(&format!("Daemon is not running (stale PID {pid})"));
            pidfile::cleanup_pidfile()?;
        }
        None => {
            user_println("Daemon is not running");
        }
    }
    Ok(())
}
