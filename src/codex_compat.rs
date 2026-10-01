use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use semver::Version;
use serde::Serialize;

pub(crate) const MINIMUM_CODEX_VERSION: &str = "0.159.2";
pub(crate) const ALIGNED_CODEX_VERSION: &str = "0.159.2";
pub(crate) const CLI_UPGRADE_NPM_COMMAND: &str = "npm install -g @openai/codex@latest";
/// A cold Windows `codex.cmd` start (Node + fnm shim) can take several seconds;
/// match the 10 s budget of the app-server help probe.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibilityStatus {
    NotChecked,
    NotFound,
    Unknown,
    /// A locally built Codex (`0.0.0`, `-dev` / `-local` suffix) whose version
    /// carries no release information, so the minimum cannot be checked.
    DevBuild,
    BelowMinimum,
    Aligned,
    AboveBaselineUnverified,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VersionReport {
    pub executable: Option<String>,
    pub version: Option<String>,
    pub status: CompatibilityStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorReport {
    pub ok: bool,
    pub minimum_version: &'static str,
    pub aligned_version: &'static str,
    pub runtime_note: &'static str,
    pub path_cli: VersionReport,
    pub desktop_codex: VersionReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_relation: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Probe {
    pub executable: PathBuf,
    pub version: Option<Version>,
    pub failure: Option<String>,
}

impl Probe {
    pub fn report(&self) -> VersionReport {
        let status = match &self.version {
            None => CompatibilityStatus::Unknown,
            Some(version) if is_dev_build_version(version) => CompatibilityStatus::DevBuild,
            Some(version) if !version_meets_minimum(version) => CompatibilityStatus::BelowMinimum,
            Some(version) if same_release_core(version, &aligned_version()) => {
                CompatibilityStatus::Aligned
            }
            Some(_) => CompatibilityStatus::AboveBaselineUnverified,
        };
        VersionReport {
            executable: Some(self.executable.display().to_string()),
            version: self.version.as_ref().map(ToString::to_string),
            status,
            note: self.failure.clone().or_else(|| match status {
                CompatibilityStatus::AboveBaselineUnverified => Some(
                    "meets the minimum version, but is newer than the currently verified baseline; full compatibility is not guaranteed".to_string(),
                ),
                CompatibilityStatus::DevBuild => Some(
                    "local development build; its version carries no release information, so the minimum version was not checked".to_string(),
                ),
                _ => None,
            }),
        }
    }

    pub fn meets_minimum(&self) -> bool {
        self.version
            .as_ref()
            .is_some_and(|version| is_dev_build_version(version) || version_meets_minimum(version))
    }
}

/// Locally built Codex binaries report `0.0.0` (or a `-dev` / `-local`
/// suffixed version), which would otherwise look older than every minimum.
pub(crate) fn is_dev_build_version(version: &Version) -> bool {
    if (version.major, version.minor, version.patch) == (0, 0, 0) {
        return true;
    }
    version
        .pre
        .as_str()
        .split('.')
        .next()
        .is_some_and(|first| matches!(first.to_ascii_lowercase().as_str(), "dev" | "local"))
}

/// Compare `major.minor.patch` only. Semver orders `0.159.2-rc.1` below
/// `0.159.2`, but a prerelease build of the minimum release already carries
/// its behavior, so it must not be refused or flagged as outdated.
fn version_meets_minimum(version: &Version) -> bool {
    (version.major, version.minor, version.patch)
        >= {
            let minimum = minimum_version();
            (minimum.major, minimum.minor, minimum.patch)
        }
}

fn same_release_core(left: &Version, right: &Version) -> bool {
    (left.major, left.minor, left.patch) == (right.major, right.minor, right.patch)
}

pub(crate) fn minimum_version() -> Version {
    Version::parse(MINIMUM_CODEX_VERSION).expect("valid minimum Codex version")
}

fn aligned_version() -> Version {
    Version::parse(ALIGNED_CODEX_VERSION).expect("valid aligned Codex version")
}

fn versions_match(left: &Version, right: &Version) -> bool {
    left.cmp_precedence(right).is_eq()
}

fn compare_versions(left: &Version, right: &Version) -> (bool, String) {
    if versions_match(left, right) {
        (true, "same".to_string())
    } else if right.cmp_precedence(left).is_gt() {
        (false, "desktop_engine_newer".to_string())
    } else {
        (false, "desktop_engine_older".to_string())
    }
}

pub(crate) fn probe_executable(path: &Path) -> Probe {
    probe_executable_with_timeout(path, VERSION_PROBE_TIMEOUT)
}

pub(crate) fn probe_executable_with_timeout(path: &Path, timeout: Duration) -> Probe {
    let resolved_path = if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    };
    let Some(resolved_path) = resolved_path else {
        return Probe {
            executable: path.to_path_buf(),
            version: None,
            failure: Some(
                "could not resolve executable path against the current directory".to_string(),
            ),
        };
    };
    let codex_home = tempfile::tempdir();
    let mut command = Command::new(&resolved_path);
    command.arg("--version");
    if let Ok(home) = &codex_home {
        command.env("CODEX_HOME", home.path());
    }
    let result = match codex_home {
        Ok(home) => {
            let result = crate::app_server::output_with_timeout(command, timeout);
            drop(home);
            result
        }
        Err(error) => Err(error),
    };
    let (version, failure) = match result {
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => (
            None,
            Some(format!(
                "version probe timed out after {} seconds",
                timeout.as_secs()
            )),
        ),
        Err(error) => (None, Some(format!("version probe failed: {error}"))),
        Ok(output) if !output.status.success() => (
            None,
            Some(format!(
                "version probe exited with {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "a signal".to_string())
            )),
        ),
        Ok(output) => match parse_version(&output.stdout, &output.stderr) {
            Some(version) => (Some(version), None),
            None => (
                None,
                Some("version probe returned no usable semantic version".to_string()),
            ),
        },
    };
    Probe {
        executable: resolved_path,
        version,
        failure,
    }
}

pub(crate) fn probe_path_cli() -> VersionReport {
    match crate::launch::command_on_path("codex") {
        Some(path) => probe_executable(&path).report(),
        None => VersionReport {
            executable: None,
            version: None,
            status: CompatibilityStatus::NotFound,
            note: Some("codex was not found on PATH".to_string()),
        },
    }
}

pub(crate) fn probe_optional_desktop(path: Option<&Path>) -> VersionReport {
    match path {
        Some(path) => probe_executable(path).report(),
        None => VersionReport {
            executable: None,
            version: None,
            status: CompatibilityStatus::NotChecked,
            note: None,
        },
    }
}

pub(crate) fn doctor_report(desktop_path: Option<&Path>) -> DoctorReport {
    let mut path_cli = probe_path_cli();
    set_below_minimum_note(&mut path_cli, path_cli_upgrade_note());
    let mut desktop_codex = probe_optional_desktop(desktop_path);
    set_below_minimum_note(&mut desktop_codex, desktop_engine_upgrade_note());
    let comparison = path_cli
        .version
        .as_deref()
        .and_then(|version| Version::parse(version).ok())
        .zip(
            desktop_codex
                .version
                .as_deref()
                .and_then(|version| Version::parse(version).ok()),
        )
        .map(|(path_version, desktop_version)| compare_versions(&path_version, &desktop_version));
    let versions_match = comparison.as_ref().map(|(matches, _)| *matches);
    let version_relation = comparison.map(|(_, relation)| relation);
    let healthy = |report: &VersionReport| {
        matches!(
            report.status,
            CompatibilityStatus::Aligned
                | CompatibilityStatus::AboveBaselineUnverified
                | CompatibilityStatus::DevBuild
        )
    };
    let ok = healthy(&path_cli)
        && (desktop_codex.status == CompatibilityStatus::NotChecked || healthy(&desktop_codex));
    DoctorReport {
        ok,
        minimum_version: MINIMUM_CODEX_VERSION,
        aligned_version: ALIGNED_CODEX_VERSION,
        runtime_note: "Matching engine versions indicate an aligned core; CLI and desktop can still expose different capabilities and use different app-server or daemon behavior.",
        path_cli,
        desktop_codex,
        versions_match,
        version_relation,
    }
}

pub(crate) fn ensure_launch_version(path: &Path) -> Result<()> {
    let probe = probe_executable(path);
    // Includes local dev builds: they cannot be compared with the minimum, so
    // they are neither refused nor warned about.
    if probe.meets_minimum() {
        return Ok(());
    }
    let report = probe.report();
    match report.status {
        CompatibilityStatus::BelowMinimum => {
            let version = report.version.as_deref().unwrap_or("unknown");
            anyhow::bail!(
                "Codex CLI at '{}' has version {version}; `codex-switch launch` requires Codex {MINIMUM_CODEX_VERSION} or newer. Upgrade using the original installation method. For npm/fnm installs, run `{CLI_UPGRADE_NPM_COMMAND}` in the same Node.js environment, restart the terminal, then verify with `codex --version` or `codex-switch doctor`.",
                path.display()
            )
        }
        CompatibilityStatus::Unknown => {
            // A slow or odd `codex --version` (cold Windows shim, wrapper
            // script) says nothing about the real version, so it must not
            // block a launch that has no other override. Only a definitively
            // parsed below-minimum version is refused.
            let reason = report
                .note
                .as_deref()
                .unwrap_or("the version probe did not return a usable version");
            tracing::warn!(path = %path.display(), "could not verify the Codex CLI version: {reason}");
            eprintln!(
                "warning: could not verify the Codex CLI version at '{}': {reason}. Continuing; `codex-switch launch` expects {MINIMUM_CODEX_VERSION} or newer. Run `codex-switch doctor` to inspect version detection.",
                path.display()
            );
            Ok(())
        }
        _ => unreachable!("a version that meets the minimum was already accepted"),
    }
}

fn set_below_minimum_note(report: &mut VersionReport, note: String) {
    if report.status == CompatibilityStatus::BelowMinimum {
        report.note = Some(note);
    }
}

fn path_cli_upgrade_note() -> String {
    format!(
        "Upgrade the Codex CLI to version {MINIMUM_CODEX_VERSION} or newer using its original installation method. For npm/fnm installs, run `{CLI_UPGRADE_NPM_COMMAND}` in the same Node.js environment; restart the terminal, then verify with `codex --version` or `codex-switch doctor`."
    )
}

fn desktop_engine_upgrade_note() -> String {
    format!(
        "The bundled Codex engine is below {MINIMUM_CODEX_VERSION}. Update the desktop app that bundles this engine to obtain a newer version, then rerun `codex-switch doctor --desktop-codex <PATH>`; npm updates only affect the PATH CLI."
    )
}

fn parse_version(stdout: &[u8], stderr: &[u8]) -> Option<Version> {
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    output.split_whitespace().find_map(|token| {
        let candidate = token.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '+')
        });
        let candidate = candidate.strip_prefix('v').unwrap_or(candidate);
        Version::parse(candidate).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_codex_cli_output() {
        assert_eq!(
            parse_version(b"codex-cli 0.159.2\n", b"").unwrap(),
            Version::new(0, 159, 2)
        );
        assert_eq!(
            parse_version(b"Codex v0.160.0\n", b"").unwrap(),
            Version::new(0, 160, 0)
        );
        assert!(parse_version(b"Codex CLI", b"").is_none());
    }

    #[cfg(unix)]
    fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(windows)]
    fn executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        path
    }

    #[test]
    fn probes_version_from_an_executable_path_with_spaces() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path(), "codex with spaces", "echo Codex CLI 0.159.2");
        let probe = probe_executable(&path);
        assert_eq!(probe.version.as_ref().unwrap(), &Version::new(0, 159, 2));
        assert!(probe.meets_minimum());
    }

    #[test]
    fn reports_failed_and_timed_out_probes_as_unknown() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let failed = executable(dir.path(), "failed-codex", "exit 7");
        let report = probe_executable(&failed).report();
        assert_eq!(report.status, CompatibilityStatus::Unknown);
        assert!(report.note.unwrap().contains("exited with 7"));

        #[cfg(unix)]
        let slow = executable(dir.path(), "slow-codex", "sleep 6; echo 0.159.2");
        #[cfg(windows)]
        let slow = {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            let powershell = PathBuf::from(root)
                .join("System32")
                .join(r"WindowsPowerShell\v1.0\powershell.exe");
            let powershell = powershell.to_string_lossy().replace('"', "\"\"");
            executable(
                dir.path(),
                "slow-codex",
                &format!(
                    "\"{powershell}\" -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 6; Write-Output 0.159.2\""
                ),
            )
        };
        let report =
            probe_executable_with_timeout(&slow, Duration::from_secs(2)).report();
        assert_eq!(report.status, CompatibilityStatus::Unknown);
        assert!(
            report
                .note
                .as_deref()
                .is_some_and(|note| note.contains("timed out")),
            "expected a timed-out version probe, got {report:?}"
        );
    }

    #[test]
    fn minimum_is_open_ended_and_future_baseline_is_unverified() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let future = executable(dir.path(), "future-codex", "echo 1.0.0");
        let probe = probe_executable(&future);
        assert!(probe.meets_minimum());
        let report = probe.report();
        assert_eq!(report.status, CompatibilityStatus::AboveBaselineUnverified);
        assert!(
            report
                .note
                .unwrap()
                .contains("full compatibility is not guaranteed")
        );

        let old = executable(dir.path(), "old-codex", "echo 0.159.1");
        assert!(!probe_executable(&old).meets_minimum());

        let release_with_build = executable(dir.path(), "build-codex", "echo 0.159.2+build.7");
        let probe = probe_executable(&release_with_build);
        assert!(probe.meets_minimum());
        assert_eq!(probe.report().status, CompatibilityStatus::Aligned);

        // A prerelease of the minimum release already has its behavior.
        let prerelease = executable(dir.path(), "prerelease-codex", "echo 0.159.2-rc.1");
        let probe = probe_executable(&prerelease);
        assert!(probe.meets_minimum());
        assert_eq!(probe.report().status, CompatibilityStatus::Aligned);
        ensure_launch_version(&prerelease).unwrap();

        let old_prerelease = executable(dir.path(), "old-pre-codex", "echo 0.159.1-rc.1");
        assert!(!probe_executable(&old_prerelease).meets_minimum());
        assert_eq!(
            probe_executable(&old_prerelease).report().status,
            CompatibilityStatus::BelowMinimum
        );
    }

    #[test]
    fn unverifiable_launch_version_warns_and_continues_but_below_minimum_still_refuses() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // Probe failure and unparseable output are both `Unknown`.
        let failed = executable(dir.path(), "failed-codex", "exit 7");
        ensure_launch_version(&failed).unwrap();
        let garbage = executable(dir.path(), "garbage-codex", "echo Codex CLI");
        ensure_launch_version(&garbage).unwrap();

        let old = executable(dir.path(), "old-codex", "echo 0.159.1");
        assert!(ensure_launch_version(&old).is_err());
    }

    #[test]
    fn dev_builds_are_not_below_minimum_and_do_not_block_launch() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for (name, output) in [
            ("zero", "codex-cli 0.0.0"),
            ("dev-suffix", "codex-cli 0.158.0-dev"),
            ("local-suffix", "codex-cli 0.158.0-local.3"),
        ] {
            let path = executable(dir.path(), name, &format!("echo {output}"));
            let probe = probe_executable(&path);
            assert!(probe.meets_minimum(), "{output}");
            let report = probe.report();
            assert_eq!(report.status, CompatibilityStatus::DevBuild, "{output}");
            assert!(report.note.unwrap().contains("development build"));
            ensure_launch_version(&path).unwrap();
        }
        // Real prereleases and old versions are unaffected.
        assert!(!is_dev_build_version(&Version::parse("0.160.0-alpha.1").unwrap()));
        assert!(!is_dev_build_version(&Version::new(0, 154, 0)));
    }

    #[test]
    fn version_probe_budget_covers_a_cold_windows_shim_start() {
        assert!(VERSION_PROBE_TIMEOUT >= Duration::from_secs(10));
    }

    #[test]
    fn below_minimum_diagnostics_give_source_specific_upgrade_steps() {
        let _env = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old = executable(dir.path(), "old-codex", "echo 0.159.1");
        let error = ensure_launch_version(&old).unwrap_err().to_string();
        assert!(error.contains("requires Codex 0.159.2 or newer"), "{error}");
        assert!(
            error.contains("npm install -g @openai/codex@latest"),
            "{error}"
        );
        assert!(error.contains("codex --version"), "{error}");
        assert!(error.contains("codex-switch doctor"), "{error}");

        let mut path_report = VersionReport {
            executable: Some("codex".to_string()),
            version: Some("0.159.1".to_string()),
            status: CompatibilityStatus::BelowMinimum,
            note: None,
        };
        set_below_minimum_note(&mut path_report, path_cli_upgrade_note());
        let path_note = path_report.note.unwrap();
        assert!(path_note.contains("npm install -g @openai/codex@latest"));
        assert!(path_note.contains("same Node.js environment"));

        let mut desktop_report = VersionReport {
            executable: Some("desktop-codex".to_string()),
            version: Some("0.159.1".to_string()),
            status: CompatibilityStatus::BelowMinimum,
            note: None,
        };
        set_below_minimum_note(&mut desktop_report, desktop_engine_upgrade_note());
        let desktop_note = desktop_report.note.unwrap();
        assert!(desktop_note.contains("Update the desktop app that bundles this engine"));
        assert!(desktop_note.contains("npm updates only affect the PATH CLI"));
        assert!(!desktop_note.contains("npm install -g"));

        let mut unknown_report = VersionReport {
            executable: Some("codex".to_string()),
            version: None,
            status: CompatibilityStatus::Unknown,
            note: Some("version probe returned no usable semantic version".to_string()),
        };
        set_below_minimum_note(&mut unknown_report, path_cli_upgrade_note());
        assert_eq!(
            unknown_report.note.as_deref(),
            Some("version probe returned no usable semantic version")
        );
    }

    #[test]
    fn build_metadata_does_not_make_engine_versions_different() {
        let release = Version::parse("0.159.2+cli-build").unwrap();
        let desktop = Version::parse("0.159.2+desktop-build").unwrap();
        assert!(versions_match(&release, &desktop));
        assert_eq!(
            compare_versions(&release, &desktop),
            (true, "same".to_string())
        );
        assert_eq!(
            compare_versions(&Version::new(0, 159, 1), &Version::new(0, 159, 2)),
            (false, "desktop_engine_newer".to_string())
        );
        assert_eq!(
            compare_versions(&Version::new(0, 159, 2), &Version::new(0, 159, 1)),
            (false, "desktop_engine_older".to_string())
        );
    }
}
