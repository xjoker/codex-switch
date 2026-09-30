use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use semver::Version;
use serde::Serialize;

pub(crate) const MINIMUM_CODEX_VERSION: &str = "0.159.2";
pub(crate) const ALIGNED_CODEX_VERSION: &str = "0.159.2";
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibilityStatus {
    NotChecked,
    NotFound,
    Unknown,
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
            Some(version) if version.cmp_precedence(&minimum_version()).is_lt() => {
                CompatibilityStatus::BelowMinimum
            }
            Some(version) if version.cmp_precedence(&aligned_version()).is_eq() => {
                CompatibilityStatus::Aligned
            }
            Some(_) => CompatibilityStatus::AboveBaselineUnverified,
        };
        VersionReport {
            executable: Some(self.executable.display().to_string()),
            version: self.version.as_ref().map(ToString::to_string),
            status,
            note: self.failure.clone().or_else(|| {
                (status == CompatibilityStatus::AboveBaselineUnverified).then(|| {
                    "meets the minimum version, but is newer than the currently verified baseline; full compatibility is not guaranteed".to_string()
                })
            }),
        }
    }

    pub fn meets_minimum(&self) -> bool {
        self.version
            .as_ref()
            .is_some_and(|version| !version.cmp_precedence(&minimum_version()).is_lt())
    }
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
    let path_cli = probe_path_cli();
    let desktop_codex = probe_optional_desktop(desktop_path);
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
            CompatibilityStatus::Aligned | CompatibilityStatus::AboveBaselineUnverified
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
    if probe.meets_minimum() {
        return Ok(());
    }
    let report = probe.report();
    let version = report.version.as_deref().unwrap_or("unknown");
    let reason = report.note.as_deref().unwrap_or("version is below minimum");
    anyhow::bail!(
        "Codex at '{}' has version {version}; codex-switch launch requires Codex {MINIMUM_CODEX_VERSION} or newer ({reason}). Run `codex-switch doctor` to inspect the PATH CLI, or install/update Codex.",
        path.display()
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
        let report = probe_executable(&slow).report();
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

        let prerelease = executable(dir.path(), "prerelease-codex", "echo 0.159.2-rc.1");
        assert!(!probe_executable(&prerelease).meets_minimum());
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
