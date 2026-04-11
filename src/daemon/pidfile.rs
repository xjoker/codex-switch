use std::path::PathBuf;

use anyhow::Result;

fn pidfile_path() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon.pid"))
}

pub fn write_pidfile() -> Result<()> {
    let path = pidfile_path()?;
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

pub fn read_pidfile() -> Option<u32> {
    let path = pidfile_path().ok()?;
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

pub fn cleanup_pidfile() -> Result<()> {
    let path = pidfile_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

pub fn is_daemon_running() -> bool {
    read_pidfile().is_some_and(process_alive)
}
