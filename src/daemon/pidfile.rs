use std::path::PathBuf;

use anyhow::Result;

fn pidfile_path() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon.pid"))
}

/// Atomically create a PID file using O_CREAT|O_EXCL semantics.
/// Fails if the file already exists (prevents TOCTOU race).
pub fn write_pidfile_exclusive() -> Result<()> {
    let path = pidfile_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // create_new(true) → O_CREAT | O_EXCL: atomic, fails if file exists
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!(
                    "PID file already exists at {}; another daemon may be running",
                    path.display()
                )
            } else {
                anyhow::anyhow!("Failed to create PID file {}: {e}", path.display())
            }
        })?;
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

pub fn read_pidfile() -> Option<u32> {
    let path = pidfile_path().ok()?;
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

pub fn cleanup_pidfile() -> Result<()> {
    let path = pidfile_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// RAII guard that cleans up the PID file on drop (including panics).
pub struct PidGuard;

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = cleanup_pidfile();
    }
}

/// Check if a process is alive using libc::kill(pid, 0).
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) only checks if the process exists; no signal is sent.
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        ret == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Send SIGTERM to a process.
pub fn send_sigterm(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: sending SIGTERM to a known PID.
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("Failed to send SIGTERM to PID {pid}: {err}");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        anyhow::bail!("Stopping daemon is only supported on Unix; use Task Manager on Windows");
    }
}

pub fn is_daemon_running() -> bool {
    read_pidfile().is_some_and(process_alive)
}
