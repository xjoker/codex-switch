//! Compatibility entry for the background daemon removed after v20260804.1.0.
//!
//! A v20260804.1.0 `self-update` stops its daemon, replaces the binary and then
//! restarts the daemon with the *new* executable. Its LaunchAgent, systemd unit
//! or scheduled task also keeps launching `codex-switch daemon start
//! --foreground`. Without this hidden command every such launch fails, and the
//! LaunchAgent's `KeepAlive` restarts it about every ten seconds forever. The
//! shim removes the old OS registration, explains the migration and exits.

#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux", windows, test))]
use std::path::PathBuf;

#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "com.codex-switch.daemon";
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT_NAME: &str = "codex-switch-daemon";
#[cfg(any(windows, test))]
const WINDOWS_TASK_NAME: &str = r"\codex-switch-daemon";

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    /// `daemon start --foreground`: launched by an old OS service or by an old
    /// `self-update` restarting its daemon.
    ServiceLaunch,
    /// The user asked for a daemon to run; that is no longer possible.
    RunRequest,
    /// `stop`, `status`, `uninstall` and anything else.
    Other,
}

fn classify(args: &[String]) -> Invocation {
    match args.first().map(String::as_str) {
        Some("start") if args.iter().any(|arg| arg == "--foreground") => Invocation::ServiceLaunch,
        Some("start" | "install" | "restart") => Invocation::RunRequest,
        _ => Invocation::Other,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cleanup {
    NothingFound,
    Removed(String),
    Failed(String),
}

/// Run the shim and return the process exit code.
pub(crate) fn run(args: &[String]) -> i32 {
    let invocation = classify(args);
    let cleanup = remove_registration(&invocation);

    eprintln!(
        "codex-switch: the background daemon was removed in this release. Account switching, \
         usage refresh and warmup run only from one-off commands and the TUI."
    );
    match &cleanup {
        Cleanup::NothingFound => eprintln!("No old daemon service registration was found."),
        Cleanup::Removed(what) => eprintln!("Removed the old daemon service: {what}."),
        Cleanup::Failed(detail) => eprintln!("Could not remove the old daemon service: {detail}"),
    }
    eprintln!(
        "Obsolete [daemon] settings in config.toml can be deleted. See \
         https://github.com/xjoker/codex-switch/wiki/Updating#migrate-from-the-removed-daemon"
    );

    // Last step: this process may itself be the service job being stopped.
    if matches!(cleanup, Cleanup::Removed(_)) || invocation == Invocation::ServiceLaunch {
        stop_running_job();
    }

    match (cleanup, invocation) {
        (Cleanup::Failed(_), _) | (_, Invocation::RunRequest) => 1,
        _ => 0,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn quiet_status(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn remove_registration(_invocation: &Invocation) -> Cleanup {
    match dirs::home_dir() {
        Some(home) => remove_launch_agent(&home),
        None => Cleanup::Failed("cannot determine the home directory".into()),
    }
}

#[cfg(target_os = "macos")]
fn stop_running_job() {
    // Removing the plist only prevents the next load; the loaded KeepAlive job
    // must be removed too. When this process is that job, launchd ends it here.
    let _ = quiet_status("/bin/launchctl", &["remove", LAUNCHD_LABEL]);
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

#[cfg(any(target_os = "macos", test))]
fn remove_launch_agent(home: &Path) -> Cleanup {
    let path = launch_agent_path(home);
    remove_file_registration(&path, || {
        format!(
            "delete {} and run `launchctl remove {LAUNCHD_LABEL}`",
            path.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn remove_registration(_invocation: &Invocation) -> Cleanup {
    let Some(home) = dirs::home_dir() else {
        return Cleanup::Failed("cannot determine the home directory".into());
    };
    remove_systemd_unit(&home, &mut |args| quiet_status("systemctl", args))
}

#[cfg(target_os = "linux")]
fn stop_running_job() {
    // --no-block returns at once; when this process is the unit, systemd stops
    // it after the report above has been written. Exit 0 would also end it.
    let _ = quiet_status(
        "systemctl",
        &["--user", "stop", "--no-block", SYSTEMD_UNIT_NAME],
    );
}

#[cfg(any(target_os = "linux", test))]
fn systemd_unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
        .join(format!("{SYSTEMD_UNIT_NAME}.service"))
}

#[cfg(any(target_os = "linux", test))]
fn remove_systemd_unit(home: &Path, systemctl: &mut dyn FnMut(&[&str]) -> bool) -> Cleanup {
    let path = systemd_unit_path(home);
    // `exists()` follows symlinks, so a unit symlinked to a target that is
    // gone would read as absent and stay registered. Look at the link itself.
    if matches!(
        std::fs::symlink_metadata(&path),
        Err(ref error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return Cleanup::NothingFound;
    }
    // A failed disable leaves only a dangling wants/ link once the unit file
    // is gone, which systemd ignores; the file removal is what matters.
    let _ = systemctl(&["--user", "disable", SYSTEMD_UNIT_NAME]);
    let result = remove_file_registration(&path, || {
        format!(
            "run `systemctl --user disable --now {SYSTEMD_UNIT_NAME}` and delete {}",
            path.display()
        )
    });
    if matches!(result, Cleanup::Removed(_)) {
        let _ = systemctl(&["--user", "daemon-reload"]);
    }
    result
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn remove_file_registration(path: &Path, manual: impl FnOnce() -> String) -> Cleanup {
    match std::fs::remove_file(path) {
        Ok(()) => Cleanup::Removed(path.display().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Cleanup::NothingFound,
        Err(error) => Cleanup::Failed(format!("{error}; manually {}", manual())),
    }
}

#[cfg(windows)]
fn schtasks_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join("schtasks.exe")
}

#[cfg(windows)]
fn remove_registration(invocation: &Invocation) -> Cleanup {
    let schtasks = schtasks_exe();
    remove_scheduled_task(invocation, &mut |args| quiet_status(&schtasks, args))
}

#[cfg(windows)]
fn stop_running_job() {
    // The task's action exits when this process returns; deleting the task
    // above already stops future logon launches.
}

#[cfg(any(windows, test))]
fn remove_scheduled_task(
    invocation: &Invocation,
    schtasks: &mut dyn FnMut(&[&str]) -> bool,
) -> Cleanup {
    if !schtasks(&["/Query", "/TN", WINDOWS_TASK_NAME]) {
        return Cleanup::NothingFound;
    }
    // Inside the task, ending it would kill this report mid-way; from a
    // terminal, a still-running old daemon must be stopped first.
    if *invocation != Invocation::ServiceLaunch {
        let _ = schtasks(&["/End", "/TN", WINDOWS_TASK_NAME]);
    }
    if schtasks(&["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"]) {
        Cleanup::Removed(format!("scheduled task {WINDOWS_TASK_NAME}"))
    } else {
        // The task was registered by this same user with a limited run level,
        // so a normal shell is the first thing to try; elevation only helps
        // when something else (a policy, another account) owns the task.
        Cleanup::Failed(format!(
            "run `schtasks /Delete /TN \"{WINDOWS_TASK_NAME}\" /F` in a normal PowerShell; if that is denied, run the same command from an elevated PowerShell"
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn remove_registration(_invocation: &Invocation) -> Cleanup {
    Cleanup::NothingFound
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn stop_running_job() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn service_launches_are_distinguished_from_requests_to_run_a_daemon() {
        assert_eq!(
            classify(&args(&["start", "--foreground"])),
            Invocation::ServiceLaunch
        );
        for request in [&["start"][..], &["install"], &["restart"]] {
            assert_eq!(classify(&args(request)), Invocation::RunRequest);
        }
        for other in [&["stop"][..], &["status", "--json"], &["uninstall"], &[]] {
            assert_eq!(classify(&args(other)), Invocation::Other);
        }
    }

    #[test]
    fn launch_agent_plist_is_removed_once() {
        let home = tempfile::tempdir().unwrap();
        let plist = launch_agent_path(home.path());
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();

        assert!(matches!(
            remove_launch_agent(home.path()),
            Cleanup::Removed(_)
        ));
        assert!(!plist.exists());
        assert_eq!(remove_launch_agent(home.path()), Cleanup::NothingFound);
    }

    #[test]
    fn systemd_unit_is_disabled_removed_and_reloaded() {
        let home = tempfile::tempdir().unwrap();
        let mut calls = Vec::new();
        assert_eq!(
            remove_systemd_unit(home.path(), &mut |args| {
                calls.push(args.join(" "));
                true
            }),
            Cleanup::NothingFound
        );
        assert!(calls.is_empty(), "no unit means no systemctl calls");

        let unit = systemd_unit_path(home.path());
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Service]\n").unwrap();
        // A failed disable must not keep the unit file alive.
        let result = remove_systemd_unit(home.path(), &mut |args| {
            calls.push(args.join(" "));
            args[1] != "disable"
        });

        assert!(matches!(result, Cleanup::Removed(_)));
        assert!(!unit.exists());
        assert_eq!(
            calls,
            ["--user disable codex-switch-daemon", "--user daemon-reload"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_unit_is_still_removed() {
        let home = tempfile::tempdir().unwrap();
        let unit = systemd_unit_path(home.path());
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.path().join("no-such-target.service"), &unit).unwrap();
        assert!(!unit.exists(), "the link must dangle for this test");

        let mut calls = Vec::new();
        let result = remove_systemd_unit(home.path(), &mut |args| {
            calls.push(args.join(" "));
            true
        });

        assert!(matches!(result, Cleanup::Removed(_)));
        assert!(std::fs::symlink_metadata(&unit).is_err());
        assert_eq!(
            calls,
            ["--user disable codex-switch-daemon", "--user daemon-reload"]
        );
    }

    #[test]
    fn scheduled_task_is_deleted_without_ending_the_task_running_the_shim() {
        let mut calls = Vec::new();
        let result = remove_scheduled_task(&Invocation::ServiceLaunch, &mut |args| {
            calls.push(args[0].to_string());
            true
        });
        assert!(matches!(result, Cleanup::Removed(_)));
        assert_eq!(calls, ["/Query", "/Delete"]);

        calls.clear();
        let result = remove_scheduled_task(&Invocation::Other, &mut |args| {
            calls.push(args[0].to_string());
            args[0] != "/Delete"
        });
        match result {
            Cleanup::Failed(detail) => {
                // The normal-shell command comes first; elevation is only the
                // fallback because the task was created unelevated.
                let normal = detail
                    .find("normal PowerShell")
                    .expect("normal shell advice");
                let elevated = detail
                    .find("elevated PowerShell")
                    .expect("elevation fallback");
                assert!(normal < elevated, "{detail}");
                assert!(detail.contains("schtasks /Delete"), "{detail}");
            }
            other => panic!("expected a failed cleanup, got {other:?}"),
        }
        assert_eq!(calls, ["/Query", "/End", "/Delete"]);

        assert_eq!(
            remove_scheduled_task(&Invocation::Other, &mut |_| false),
            Cleanup::NothingFound
        );
    }
}
