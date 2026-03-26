//! Detect running Codex CLI processes.

use tracing::debug;

/// Names of Codex-related processes to detect.
const CODEX_PROCESS_NAMES: &[&str] = &["codex"];

/// Detected Codex process info.
#[derive(Debug)]
pub struct CodexProcess {
    pub pid: u32,
    pub name: String,
}

/// Check if any Codex CLI processes are currently running.
/// Returns a list of detected processes (empty if none found).
pub fn detect_codex_processes() -> Vec<CodexProcess> {
    let mut found = Vec::new();

    #[cfg(unix)]
    {
        // Use `pgrep -x` for exact process name matching
        for name in CODEX_PROCESS_NAMES {
            if let Ok(output) = std::process::Command::new("pgrep")
                .args(["-x", name])
                .output()
            {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    for line in stdout.lines() {
                        if let Ok(pid) = line.trim().parse::<u32>() {
                            debug!("Detected codex process: pid={pid} name={name}");
                            found.push(CodexProcess {
                                pid,
                                name: name.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    {
        // Use tasklist to find codex processes
        if let Ok(output) = std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let lower = line.to_lowercase();
                    for name in CODEX_PROCESS_NAMES {
                        if lower.contains(&format!("\"{name}.exe\""))
                            || lower.contains(&format!("\"{name}\""))
                        {
                            // CSV format: "process.exe","PID",...
                            if let Some(pid_str) = line.split(',').nth(1) {
                                let pid_str = pid_str.trim().trim_matches('"');
                                if let Ok(pid) = pid_str.parse::<u32>() {
                                    debug!("Detected codex process: pid={pid} name={name}");
                                    found.push(CodexProcess {
                                        pid,
                                        name: name.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    found
}
