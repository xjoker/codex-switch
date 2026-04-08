use anyhow::Result;
use std::path::PathBuf;
use crate::output::user_println;

pub fn install() -> Result<()> {
    #[cfg(target_os = "macos")]
    return install_launchd();
    #[cfg(target_os = "linux")]
    return install_systemd();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("Service install is not supported on this platform")
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    return uninstall_launchd();
    #[cfg(target_os = "linux")]
    return uninstall_systemd();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("Service uninstall is not supported on this platform")
}

// -- macOS LaunchAgent --

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join("Library/LaunchAgents/com.codex-switch.daemon.plist"))
}

#[cfg(target_os = "macos")]
fn install_launchd() -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();
    let log_dir = crate::auth::app_home()?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.codex-switch.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_out}</string>
    <key>StandardErrorPath</key>
    <string>{log_err}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HOME</key>
        <string>{home}</string>
    </dict>
</dict>
</plist>"#,
        exe = exe,
        home = home,
        log_out = log_dir.join("daemon.log").display(),
        log_err = log_dir.join("daemon.err").display(),
    );

    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;

    let status = std::process::Command::new("launchctl")
        .args(["load", &path.display().to_string()])
        .status()?;
    if !status.success() {
        anyhow::bail!("launchctl load failed");
    }
    user_println(&format!("Installed LaunchAgent at {}", path.display()));
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd() -> Result<()> {
    let path = plist_path()?;
    if !path.exists() {
        user_println("LaunchAgent not installed");
        return Ok(());
    }
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.display().to_string()])
        .status();
    std::fs::remove_file(&path)?;
    user_println("Uninstalled LaunchAgent");
    Ok(())
}

// -- Linux systemd --

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(home.join(".config/systemd/user/codex-switch-daemon.service"))
}

#[cfg(target_os = "linux")]
fn install_systemd() -> Result<()> {
    let exe = std::env::current_exe()?.display().to_string();
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .display()
        .to_string();

    let unit = format!(
        r#"[Unit]
Description=codex-switch auto-switching daemon
After=network-online.target

[Service]
Type=simple
ExecStart={exe} daemon start --foreground
Restart=on-failure
RestartSec=10
Environment=HOME={home}

[Install]
WantedBy=default.target
"#,
        exe = exe,
        home = home,
    );

    let path = unit_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, unit)?;

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let status = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", "codex-switch-daemon"])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl enable failed");
    }
    user_println(&format!("Installed systemd user service at {}", path.display()));
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<()> {
    let path = unit_path()?;
    if !path.exists() {
        user_println("systemd service not installed");
        return Ok(());
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "codex-switch-daemon"])
        .status();
    std::fs::remove_file(&path)?;
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    user_println("Uninstalled systemd user service");
    Ok(())
}
