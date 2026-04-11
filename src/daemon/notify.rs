/// Send a desktop notification. Best-effort, never fails.
pub fn send_notification(message: &str) {
    // Sanitize: keep only printable ASCII, no control chars or AppleScript metacharacters
    let safe: String = message
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(200)
        .collect();

    #[cfg(target_os = "macos")]
    {
        // Escape both backslashes and quotes for AppleScript string safety
        let escaped = safe.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "display notification \"{escaped}\" with title \"codex-switch\""
                ),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args(["codex-switch", &safe])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = safe;
    }
}
