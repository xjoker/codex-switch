use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn installer(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

fn clear_cs_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CS_") {
            command.env_remove(key);
        }
    }
}

#[cfg(windows)]
fn isolated_windows_installer(root: &Path) -> PathBuf {
    const GET_USER_PATH: &str = "[Environment]::GetEnvironmentVariable(\"Path\", \"User\")";
    const SET_USER_PATH: &str =
        "[Environment]::SetEnvironmentVariable(\"Path\", $NewPath, \"User\")";
    let source = fs::read_to_string(installer("install.ps1")).unwrap();
    assert_eq!(source.matches(GET_USER_PATH).count(), 2);
    assert_eq!(source.matches(SET_USER_PATH).count(), 2);
    let isolated = source
        .replace(GET_USER_PATH, "$env:MOCK_USER_PATH")
        .replace(SET_USER_PATH, "$env:MOCK_USER_PATH = $NewPath");
    assert!(!isolated.contains(GET_USER_PATH));
    assert!(!isolated.contains(SET_USER_PATH));

    let path = root.join("install.ps1");
    fs::write(&path, isolated).unwrap();
    path
}

#[cfg(windows)]
fn powershell_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

#[cfg(unix)]
#[test]
fn unix_uninstall_does_not_invoke_installed_binary_without_a_native_service() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let install_dir = home.join(".local").join("bin");
    let marker = root.path().join("daemon-invoked");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&install_dir).unwrap();

    let binary = install_dir.join("codex-switch");
    fs::write(
        &binary,
        "#!/bin/sh\nprintf invoked > \"$MOCK_DAEMON_MARKER\"\nexit 42\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        install_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg(installer("install.sh"))
        .arg("--uninstall")
        .env("HOME", &home)
        .env("PATH", path)
        .env("MOCK_DAEMON_MARKER", &marker)
        .stdin(Stdio::null());
    clear_cs_environment(&mut command);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "uninstall invoked the installed binary");
    assert!(
        !binary.exists(),
        "uninstall did not remove the installed binary"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unix_uninstall_keeps_binary_when_native_service_stop_fails() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let install_dir = home.join(".local").join("bin");
    let mock_bin = root.path().join("mock-bin");
    let marker = root.path().join("systemctl-invoked");
    let unit = home
        .join(".config")
        .join("systemd")
        .join("user")
        .join("codex-switch-daemon.service");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::create_dir_all(&mock_bin).unwrap();
    fs::write(&unit, b"[Unit]\n").unwrap();

    let binary = install_dir.join("codex-switch");
    fs::write(&binary, b"installed binary").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let systemctl = mock_bin.join("systemctl");
    fs::write(
        &systemctl,
        "#!/bin/sh\nprintf invoked > \"$MOCK_SYSTEMCTL_MARKER\"\nexit 42\n",
    )
    .unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}:{}",
        mock_bin.display(),
        install_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg(installer("install.sh"))
        .arg("--uninstall")
        .env("HOME", &home)
        .env("PATH", path)
        .env("MOCK_SYSTEMCTL_MARKER", &marker)
        .stdin(Stdio::null());
    clear_cs_environment(&mut command);
    let output = command.output().unwrap();

    assert!(!output.status.success());
    assert!(marker.exists(), "native service cleanup was not attempted");
    assert!(
        binary.exists(),
        "binary was removed after native service cleanup failed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_uninstall_keeps_binary_when_launch_agent_stop_fails() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let install_dir = home.join(".local").join("bin");
    let mock_bin = root.path().join("mock-bin");
    let marker = root.path().join("launchctl-invoked");
    let plist = home
        .join("Library")
        .join("LaunchAgents")
        .join("com.codex-switch.daemon.plist");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    fs::create_dir_all(&mock_bin).unwrap();
    fs::write(&plist, b"<?xml version=\"1.0\"?>\n").unwrap();

    let binary = install_dir.join("codex-switch");
    fs::write(&binary, b"installed binary").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let launchctl = mock_bin.join("launchctl");
    fs::write(
        &launchctl,
        "#!/bin/sh\nprintf invoked > \"$MOCK_LAUNCHCTL_MARKER\"\nexit 42\n",
    )
    .unwrap();
    fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}:{}",
        mock_bin.display(),
        install_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg(installer("install.sh"))
        .arg("--uninstall")
        .env("HOME", &home)
        .env("PATH", path)
        .env("MOCK_LAUNCHCTL_MARKER", &marker)
        .stdin(Stdio::null());
    clear_cs_environment(&mut command);
    let output = command.output().unwrap();

    assert!(!output.status.success());
    assert!(marker.exists(), "LaunchAgent cleanup was not attempted");
    assert!(
        binary.exists(),
        "binary was removed after LaunchAgent cleanup failed"
    );
}

#[cfg(windows)]
#[test]
fn windows_uninstall_does_not_invoke_installed_binary_without_a_scheduled_task() {
    let root = tempfile::tempdir().unwrap();
    let local_app_data = root.path().join("local-app-data");
    let user_profile = root.path().join("user-profile");
    let temp_dir = root.path().join("temp");
    let install_dir = local_app_data.join("Programs").join("codex-switch");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&user_profile).unwrap();
    fs::create_dir_all(&temp_dir).unwrap();

    let installed = install_dir.join("codex-switch.exe");
    fs::copy(env!("CARGO_BIN_EXE_codex-switch"), &installed).unwrap();

    let isolated_script = isolated_windows_installer(root.path());

    let shell = ["pwsh", "powershell"]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("a PowerShell host is required for install.ps1");
    let script_literal = powershell_literal(&isolated_script);
    let command = format!(
        "function global:schtasks.exe {{ $global:LASTEXITCODE = 0 }}; $env:CS_UNINSTALL = '1'; & {script_literal}"
    );
    let mut process = Command::new(shell);
    process
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .env("LOCALAPPDATA", &local_app_data)
        .env("USERPROFILE", &user_profile)
        .env("TEMP", &temp_dir)
        .env("TMP", &temp_dir)
        .env("MOCK_USER_PATH", &install_dir)
        .stdin(Stdio::null());
    clear_cs_environment(&mut process);
    let output = process.output().unwrap();

    assert!(
        output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !installed.exists(),
        "uninstall invoked the installed binary"
    );
}

#[cfg(windows)]
#[test]
fn windows_uninstall_removes_idle_scheduled_task_when_end_reports_not_running() {
    let root = tempfile::tempdir().unwrap();
    let local_app_data = root.path().join("local-app-data");
    let user_profile = root.path().join("user-profile");
    let temp_dir = root.path().join("temp");
    let install_dir = local_app_data.join("Programs").join("codex-switch");
    let end_marker = root.path().join("scheduled-task-end-invoked");
    let delete_marker = root.path().join("scheduled-task-delete-invoked");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&user_profile).unwrap();
    fs::create_dir_all(&temp_dir).unwrap();

    let installed = install_dir.join("codex-switch.exe");
    fs::copy(env!("CARGO_BIN_EXE_codex-switch"), &installed).unwrap();

    let isolated_script = isolated_windows_installer(root.path());
    let shell = ["pwsh", "powershell"]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("a PowerShell host is required for install.ps1");
    let script_literal = powershell_literal(&isolated_script);
    let end_literal = powershell_literal(&end_marker);
    let delete_literal = powershell_literal(&delete_marker);
    // Signed HRESULT 0x8004130B: SCHED_E_TASK_NOT_RUNNING.
    let task_not_running_hresult = -2_147_216_629_i64;
    let command = format!(
        "function global:schtasks.exe {{ if ($args -contains '/Query') {{ Write-Output '\"\\codex-switch-daemon\",\"N/A\",\"Ready\"'; $global:LASTEXITCODE = 0 }} elseif ($args -contains '/End') {{ if ($args -contains '/HRESULT') {{ Set-Content -LiteralPath {end_literal} -Value 'end'; $global:LASTEXITCODE = {task_not_running_hresult} }} else {{ $global:LASTEXITCODE = 87 }} }} elseif ($args -contains '/Delete') {{ Set-Content -LiteralPath {delete_literal} -Value 'delete'; $global:LASTEXITCODE = 0 }} }}; $env:CS_UNINSTALL = '1'; & {script_literal}"
    );
    let mut process = Command::new(shell);
    process
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .env("LOCALAPPDATA", &local_app_data)
        .env("USERPROFILE", &user_profile)
        .env("TEMP", &temp_dir)
        .env("TMP", &temp_dir)
        .env("MOCK_USER_PATH", &install_dir)
        .stdin(Stdio::null());
    clear_cs_environment(&mut process);
    let output = process.output().unwrap();

    assert!(
        output.status.success(),
        "idle-task uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(end_marker.exists(), "idle-task stop was not attempted");
    assert!(delete_marker.exists(), "idle task was not deleted");
    assert!(
        !installed.exists(),
        "binary was not removed after idle cleanup"
    );
}

#[cfg(windows)]
#[test]
fn windows_uninstall_via_iex_keeps_host_alive_after_success() {
    let root = tempfile::tempdir().unwrap();
    let local_app_data = root.path().join("local-app-data");
    let user_profile = root.path().join("user-profile");
    let temp_dir = root.path().join("temp");
    let install_dir = local_app_data.join("Programs").join("codex-switch");
    let alive_marker = root.path().join("iex-host-alive");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&user_profile).unwrap();
    fs::create_dir_all(&temp_dir).unwrap();

    let installed = install_dir.join("codex-switch.exe");
    fs::copy(env!("CARGO_BIN_EXE_codex-switch"), &installed).unwrap();

    let isolated_script = isolated_windows_installer(root.path());
    let shell = ["pwsh", "powershell"]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("a PowerShell host is required for install.ps1");
    let script_literal = powershell_literal(&isolated_script);
    let alive_literal = powershell_literal(&alive_marker);
    let command = format!(
        "function global:schtasks.exe {{ $global:LASTEXITCODE = 0 }}; $env:CS_UNINSTALL = '1'; $script = Get-Content -Raw -LiteralPath {script_literal}; Invoke-Expression $script; Set-Content -LiteralPath {alive_literal} -Value 'alive'"
    );
    let mut process = Command::new(shell);
    process
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .env("LOCALAPPDATA", &local_app_data)
        .env("USERPROFILE", &user_profile)
        .env("TEMP", &temp_dir)
        .env("TMP", &temp_dir)
        .env("MOCK_USER_PATH", &install_dir)
        .stdin(Stdio::null());
    clear_cs_environment(&mut process);
    let output = process.output().unwrap();

    assert!(
        output.status.success(),
        "IEX uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        alive_marker.exists(),
        "IEX closed the host before continuation"
    );
    assert!(
        !installed.exists(),
        "IEX uninstall did not remove the binary"
    );
}

#[cfg(windows)]
#[test]
fn windows_uninstall_keeps_binary_when_scheduled_task_stop_fails() {
    let root = tempfile::tempdir().unwrap();
    let local_app_data = root.path().join("local-app-data");
    let user_profile = root.path().join("user-profile");
    let temp_dir = root.path().join("temp");
    let install_dir = local_app_data.join("Programs").join("codex-switch");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&user_profile).unwrap();
    fs::create_dir_all(&temp_dir).unwrap();

    let installed = install_dir.join("codex-switch.exe");
    fs::copy(env!("CARGO_BIN_EXE_codex-switch"), &installed).unwrap();

    let isolated_script = isolated_windows_installer(root.path());

    let shell = ["pwsh", "powershell"]
        .into_iter()
        .find(|candidate| {
            Command::new(candidate)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("a PowerShell host is required for install.ps1");
    let script_literal = powershell_literal(&isolated_script);
    let command = format!(
        "function global:schtasks.exe {{ if ($args -contains '/Query') {{ Write-Output '\"\\codex-switch-daemon\",\"N/A\",\"Ready\"'; $global:LASTEXITCODE = 0 }} elseif ($args -contains '/End') {{ $global:LASTEXITCODE = 5 }} }}; $env:CS_UNINSTALL = '1'; & {script_literal}"
    );
    let mut process = Command::new(shell);
    process
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .env("LOCALAPPDATA", &local_app_data)
        .env("USERPROFILE", &user_profile)
        .env("TEMP", &temp_dir)
        .env("TMP", &temp_dir)
        .env("MOCK_USER_PATH", &install_dir)
        .stdin(Stdio::null());
    clear_cs_environment(&mut process);
    let output = process.output().unwrap();

    assert!(!output.status.success());
    assert!(
        installed.exists(),
        "binary was removed after scheduled task cleanup failed"
    );
}
