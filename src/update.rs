use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http_retry::{self, ReplaySafety};

const REPO_OWNER: &str = "xjoker";
const REPO_NAME: &str = "codex-switch";
const BIN_NAME: &str = "codex-switch";
const PROVENANCE_ASSET_NAME: &str = "codex-switch-build-provenance.json";
const RELEASE_WORKFLOW: &str = "xjoker/codex-switch/.github/workflows/release.yml";
const SYSTEM_INSTALL_DIR: &str = "/usr/local/bin";
const SYSTEM_INSTALL_MARKER_NAME: &str = ".codex-switch-system-install-v1";
const UPDATE_TTL_SECS: i64 = 12 * 60 * 60;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct LegacySystemInstallMigrationRequired {
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePlatform {
    Unix,
    Windows,
}

fn current_update_platform() -> UpdatePlatform {
    if cfg!(windows) {
        UpdatePlatform::Windows
    } else {
        UpdatePlatform::Unix
    }
}

fn unix_migration_command(release_tag: &str) -> String {
    let safe_tag = !release_tag.is_empty()
        && release_tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    let url = if safe_tag {
        format!(
            "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{release_tag}/install.sh"
        )
    } else {
        format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh")
    };
    if release_tag == "dev" && safe_tag {
        format!("`curl -fsSL {url} | bash -s -- --dev`")
    } else {
        format!("`curl -fsSL {url} | bash`")
    }
}

fn legacy_system_install_migration_hint(
    executable: &Path,
    platform: UpdatePlatform,
    marker_present: bool,
    use_dev: bool,
    requested_version: Option<&str>,
) -> Option<String> {
    if platform != UpdatePlatform::Unix
        || executable.parent() != Some(Path::new(SYSTEM_INSTALL_DIR))
        || marker_present
    {
        return None;
    }

    let exact_version = requested_version.map(normalize_version);
    if exact_version
        .as_deref()
        .is_some_and(|version| Version::parse(version).is_err())
    {
        return Some(format!(
            "One-time setup could not start\n\nThe requested version is not valid. Use a semantic version such as `20260712.2.0`, then retry. The existing installation at '{}' was not changed.",
            executable.display()
        ));
    }

    let (user_command, system_command) = if let Some(version) = exact_version {
        let url = format!(
            "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/v{version}/install.sh"
        );
        (
            format!("curl -fsSL {url} | CS_VERSION={version} bash"),
            format!("curl -fsSL {url} | CS_VERSION={version} bash -s -- --system"),
        )
    } else if use_dev {
        (
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/dev/install.sh | bash -s -- --dev"
            ),
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/dev/install.sh | bash -s -- --dev --system"
            ),
        )
    } else {
        (
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh | bash"
            ),
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh | bash -s -- --system"
            ),
        )
    };

    Some(format!(
        "One-time setup required\n\ncodex-switch is still installed system-wide at '{}'. Choose how future updates should work.\n\nRecommended — move it to your user account:\n  {user_command}\n\nProfiles and configuration are preserved. Future updates will not need sudo.\n\nKeep the system-wide install instead:\n  {system_command}\n\nFuture updates will continue to require sudo.",
        executable.display()
    ))
}

fn canonical_executable_path(executable: PathBuf) -> PathBuf {
    fs::canonicalize(&executable).unwrap_or(executable)
}

pub fn ensure_legacy_system_install_migrated(
    use_dev: bool,
    requested_version: Option<&str>,
) -> Result<()> {
    let executable =
        canonical_executable_path(std::env::current_exe().context("locating current executable")?);
    let platform = current_update_platform();
    let marker_present = executable
        .parent()
        .map(|parent| parent.join(SYSTEM_INSTALL_MARKER_NAME).is_file())
        .unwrap_or(false);

    if let Some(hint) = legacy_system_install_migration_hint(
        &executable,
        platform,
        marker_present,
        use_dev,
        requested_version,
    ) {
        return Err(LegacySystemInstallMigrationRequired { message: hint }.into());
    }
    Ok(())
}

fn replacement_permission_hint(
    executable: &Path,
    platform: UpdatePlatform,
    release_tag: &str,
) -> String {
    let parent = executable.parent().unwrap_or(executable);
    match platform {
        UpdatePlatform::Unix if parent == Path::new(SYSTEM_INSTALL_DIR) => format!(
            "install directory '{}' is not writable; for a legacy direct install, rerun the user-level installer once with {}. If codex-switch was intentionally installed with `--system`, run `sudo codex-switch self-update` instead",
            parent.display(),
            unix_migration_command(release_tag)
        ),
        UpdatePlatform::Unix => format!(
            "user-owned install directory '{}' is not writable; fix its ownership or reinstall with the user-level installer. Do not run self-update with elevated privileges",
            parent.display()
        ),
        UpdatePlatform::Windows => format!(
            "install directory '{}' is not writable; close running codex-switch processes and retry, or reinstall with the user-level installer",
            parent.display()
        ),
    }
}

pub(crate) fn homebrew_dev_install_hint() -> &'static str {
    "run `brew uninstall codex-switch`, then follow the development-release instructions at https://github.com/xjoker/codex-switch/wiki/Development-Releases#install-the-rolling-dev-build"
}

fn homebrew_dev_install_error() -> String {
    format!(
        "codex-switch is installed via Homebrew. To switch to dev, {}.",
        homebrew_dev_install_hint()
    )
}

fn ensure_replace_parent_writable(
    executable: &Path,
    platform: UpdatePlatform,
    release_tag: &str,
) -> Result<()> {
    let parent = executable
        .parent()
        .with_context(|| format!("current executable has no parent: {}", executable.display()))?;
    tempfile::NamedTempFile::new_in(parent)
        .with_context(|| replacement_permission_hint(executable, platform, release_tag))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    Homebrew,
    Direct,
}

impl InstallSource {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallSource::Homebrew => "homebrew",
            InstallSource::Direct => "direct",
        }
    }

    pub fn upgrade_hint(self) -> &'static str {
        match self {
            InstallSource::Homebrew => "brew upgrade xjoker/tap/codex-switch",
            InstallSource::Direct => "codex-switch self-update",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub install_source: InstallSource,
}

#[derive(Debug, Clone)]
pub struct SelfUpdateResult {
    pub current_version: String,
    pub latest_version: String,
    pub install_source: InstallSource,
    pub updated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: i64,
    latest_version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitReference {
    object: GithubGitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitTag {
    object: GithubGitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check_for_update(force: bool) -> Result<Option<UpdateInfo>> {
    let current_version = current_version().to_string();
    let latest_version = latest_release_version(force).await?;
    if !is_newer_version(&latest_version, &current_version) {
        return Ok(None);
    }

    Ok(Some(UpdateInfo {
        current_version,
        latest_version,
        install_source: detect_install_source(),
    }))
}

/// Check whether a newer dev release exists on GitHub.
///
/// Dev versions use a `dev` pre-release component. Older timestamped dev
/// versions remain supported for updates from existing installations.
pub async fn check_for_dev_update() -> Result<Option<UpdateInfo>> {
    let current_version = current_version().to_string();
    let release = match fetch_release_optional(Some("dev"))
        .await
        .context("checking dev release")?
    {
        Some(r) => r,
        None => return Ok(None), // No dev release exists (404).
    };
    let dev_version = extract_release_version(&release);
    if !is_dev_update_available(&dev_version, &current_version) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        current_version,
        latest_version: dev_version,
        install_source: detect_install_source(),
    }))
}

pub async fn self_update(version: Option<&str>, show_progress: bool) -> Result<SelfUpdateResult> {
    // Before anything reaches the network: the argument becomes part of a
    // GitHub API path, so it is rejected here rather than encoded and sent.
    let requested_version = version.map(validate_requested_version).transpose()?;

    let install_source = detect_install_source();
    if install_source == InstallSource::Homebrew {
        anyhow::bail!(
            "Homebrew-managed install detected. Run `{}` instead.",
            install_source.upgrade_hint()
        );
    }

    let current_version = current_version().to_string();
    let release = fetch_release(requested_version.as_deref()).await?;
    let latest_version = extract_release_version(&release);

    if let Some(requested) = requested_version {
        if requested != latest_version {
            anyhow::bail!("requested version '{requested}' was not found on GitHub Releases");
        }
        if is_older_version(&latest_version, &current_version) {
            anyhow::bail!(
                "downgrades are not supported: requested version {latest_version} is older than current version {current_version}"
            );
        }
        if latest_version == current_version {
            return Ok(SelfUpdateResult {
                current_version,
                latest_version,
                install_source,
                updated: false,
            });
        }
    } else if !is_newer_version(&latest_version, &current_version) {
        return Ok(SelfUpdateResult {
            current_version,
            latest_version,
            install_source,
            updated: false,
        });
    }

    download_and_replace(&release, show_progress, "").await?;

    save_update_cache(&UpdateCache {
        checked_at: crate::auth::now_unix_secs(),
        latest_version: latest_version.clone(),
    });

    Ok(SelfUpdateResult {
        current_version,
        latest_version,
        install_source,
        updated: true,
    })
}

/// Install the dev build from the `dev` GitHub Release tag.
///
/// Switching from dev→stable uses the normal `self_update` path.
pub async fn self_update_dev(show_progress: bool) -> Result<SelfUpdateResult> {
    let install_source = detect_install_source();
    if install_source == InstallSource::Homebrew {
        anyhow::bail!(homebrew_dev_install_error());
    }

    let current_version = current_version().to_string();
    let release = fetch_release(Some("dev"))
        .await
        .context("fetching dev release from GitHub")?;
    let dev_version = extract_release_version(&release);

    if !is_dev_update_available(&dev_version, &current_version) {
        return Ok(SelfUpdateResult {
            current_version,
            latest_version: dev_version,
            install_source,
            updated: false,
        });
    }

    download_and_replace(&release, show_progress, " (dev)").await?;

    Ok(SelfUpdateResult {
        current_version,
        latest_version: dev_version,
        install_source,
        updated: true,
    })
}

/// Extract a semver-compatible version string from a GitHub Release.
///
/// For dev releases (`is_dev = true`) the version is embedded in the release
/// name (e.g. `"dev (20260712.1.0-dev)"`) because the tag itself is just
/// `"dev"`. For stable releases the tag carries the version directly.
fn extract_release_version(release: &GithubRelease) -> String {
    // Dev releases carry the version in the name: "dev (X.Y.Z-dev)"
    if release.tag_name == "dev"
        && let Some(v) = release
            .name
            .as_deref()
            .and_then(|n| n.strip_prefix("dev ("))
            .and_then(|n| n.strip_suffix(')'))
        && Version::parse(v).is_ok()
    {
        return v.to_string();
    }
    normalize_version(&release.tag_name)
}

/// Download, verify, extract and replace the current binary from a GitHub Release.
async fn download_and_replace(
    release: &GithubRelease,
    show_progress: bool,
    label_suffix: &str,
) -> Result<()> {
    let executable = std::env::current_exe().context("locating current executable")?;
    let platform = current_update_platform();
    ensure_replace_parent_writable(&executable, platform, &release.tag_name)?;
    let client =
        crate::auth::build_http_client().context("building HTTP client for self-update")?;
    let archive_name = asset_name();
    let archive_asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .cloned()
        .with_context(|| format!("release does not contain asset '{archive_name}'"))?;
    let checksum_name = format!("{archive_name}.sha256");
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .cloned()
        .with_context(|| format!("release does not contain checksum asset '{checksum_name}'"))?;
    let provenance_asset = release
        .assets
        .iter()
        .find(|a| a.name == PROVENANCE_ASSET_NAME)
        .cloned()
        .with_context(|| {
            format!("release does not contain provenance asset '{PROVENANCE_ASSET_NAME}'")
        })?;

    let temp_dir = tempfile::tempdir().context("creating temporary update directory")?;
    let archive_path = temp_dir.path().join(&archive_asset.name);
    let provenance_path = temp_dir.path().join(PROVENANCE_ASSET_NAME);
    if show_progress {
        eprintln!("Downloading {}{}...", archive_asset.name, label_suffix);
    }
    download_file(&client, &archive_asset.browser_download_url, &archive_path).await?;
    verify_checksum(&client, &checksum_asset.browser_download_url, &archive_path).await?;
    download_file(
        &client,
        &provenance_asset.browser_download_url,
        &provenance_path,
    )
    .await?;
    let source_digest = fetch_tag_commit_sha(&client, &release.tag_name).await?;
    verify_build_provenance(
        &archive_path,
        &provenance_path,
        &release.tag_name,
        &source_digest,
    )?;
    let confirmed_digest = fetch_tag_commit_sha(&client, &release.tag_name).await?;
    if confirmed_digest != source_digest {
        anyhow::bail!(
            "release tag '{}' moved from {source_digest} to {confirmed_digest} during update; \
             refusing to replace the executable",
            release.tag_name
        );
    }

    let extracted_path = temp_dir.path().join(extracted_binary_name());
    if show_progress {
        eprintln!("Extracting update package...");
    }
    extract_binary(&archive_path, &extracted_path)?;

    if show_progress {
        eprintln!("Replacing current executable...");
    }
    self_replace::self_replace(&extracted_path).with_context(|| {
        format!(
            "replacing current executable: {}",
            replacement_permission_hint(&executable, platform, &release.tag_name)
        )
    })?;
    Ok(())
}

fn attestation_verify_args(
    archive_path: &Path,
    bundle_path: &Path,
    release_tag: &str,
    source_digest: &str,
) -> Vec<String> {
    vec![
        "attestation".to_string(),
        "verify".to_string(),
        archive_path.to_string_lossy().into_owned(),
        "--bundle".to_string(),
        bundle_path.to_string_lossy().into_owned(),
        "--repo".to_string(),
        format!("{REPO_OWNER}/{REPO_NAME}"),
        "--signer-workflow".to_string(),
        RELEASE_WORKFLOW.to_string(),
        "--source-ref".to_string(),
        format!("refs/tags/{release_tag}"),
        "--source-digest".to_string(),
        source_digest.to_string(),
        "--deny-self-hosted-runners".to_string(),
    ]
}

fn verify_build_provenance(
    archive_path: &Path,
    bundle_path: &Path,
    release_tag: &str,
    source_digest: &str,
) -> Result<()> {
    let args = attestation_verify_args(archive_path, bundle_path, release_tag, source_digest);
    let output = std::process::Command::new("gh")
        .args(&args)
        .output()
        .with_context(|| {
            "running `gh attestation verify`; install a GitHub CLI version with attestation \
             support before using self-update"
        })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::bail!(
        "release provenance verification failed for {}: {}",
        archive_path.display(),
        if detail.is_empty() {
            "gh attestation verify returned a non-zero status"
        } else {
            detail
        }
    )
}

/// Returns true if the given version string contains a pre-release component
/// (e.g. `20260712.1.0-dev`; legacy timestamped versions also match).
pub fn is_dev_version(version: &str) -> bool {
    normalize_version(version).contains("-dev")
}

pub fn detect_install_source() -> InstallSource {
    let exe = std::env::current_exe().ok();
    let exe = exe
        .as_ref()
        .and_then(|path| fs::canonicalize(path).ok())
        .or(exe)
        .unwrap_or_else(|| PathBuf::from(BIN_NAME));
    let path = exe.to_string_lossy().replace('\\', "/");

    if path.contains("/Cellar/codex-switch/") || path.contains("/Homebrew/Cellar/codex-switch/") {
        InstallSource::Homebrew
    } else {
        InstallSource::Direct
    }
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn should_show_download_progress() -> bool {
    io::stderr().is_terminal()
}

async fn latest_release_version(force: bool) -> Result<String> {
    if !force
        && let Some(cache) = load_update_cache()
        && crate::auth::now_unix_secs() - cache.checked_at <= update_ttl_secs()
    {
        return Ok(cache.latest_version);
    }

    let release = fetch_release(None).await?;
    let latest_version = normalize_version(&release.tag_name);
    save_update_cache(&UpdateCache {
        checked_at: crate::auth::now_unix_secs(),
        latest_version: latest_version.clone(),
    });
    Ok(latest_version)
}

async fn fetch_release(version: Option<&str>) -> Result<GithubRelease> {
    fetch_release_inner(version)
        .await?
        .ok_or_else(|| anyhow::anyhow!("release not found"))
}

/// Fetch a GitHub Release, returning `Ok(None)` for 404 (release not found)
/// and propagating all other errors.
async fn fetch_release_optional(version: Option<&str>) -> Result<Option<GithubRelease>> {
    fetch_release_inner(version).await
}

async fn fetch_release_inner(version: Option<&str>) -> Result<Option<GithubRelease>> {
    let client =
        crate::auth::build_http_client().context("building HTTP client for update check")?;
    let url = release_api_url(version);
    let resp = http_retry::send(
        client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"),
        ReplaySafety::Idempotent,
    )
    .await
    .context("requesting GitHub release metadata")?;

    if resp.status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !resp.status.is_success() {
        anyhow::bail!("GitHub release request failed (HTTP {})", resp.status);
    }
    let release = serde_json::from_slice::<GithubRelease>(&resp.body)
        .context("parsing GitHub release metadata")?;
    Ok(Some(release))
}

async fn fetch_tag_commit_sha(client: &reqwest::Client, tag: &str) -> Result<String> {
    let reference = fetch_github_json::<GithubGitReference>(
        client,
        &tag_ref_api_url(tag),
        "requesting GitHub release tag reference",
    )
    .await?;
    let mut object = reference.object;
    for _ in 0..5 {
        match object.kind.as_str() {
            "commit" => {
                validate_commit_sha(&object.sha)?;
                return Ok(object.sha.to_ascii_lowercase());
            }
            "tag" => {
                let tag_object = fetch_github_json::<GithubGitTag>(
                    client,
                    &git_tag_api_url(&object.sha),
                    "resolving annotated GitHub release tag",
                )
                .await?;
                object = tag_object.object;
            }
            other => anyhow::bail!(
                "release tag '{tag}' resolved to unsupported Git object type '{other}'"
            ),
        }
    }
    anyhow::bail!("release tag '{tag}' contains more than 5 nested annotated tags")
}

async fn fetch_github_json<T>(client: &reqwest::Client, url: &str, context: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = http_retry::send(
        client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"),
        ReplaySafety::Idempotent,
    )
    .await
    .with_context(|| context.to_string())?;
    if !response.status.is_success() {
        anyhow::bail!("{context}: {url}: HTTP {}", response.status);
    }
    serde_json::from_slice::<T>(&response.body)
        .with_context(|| format!("parsing GitHub response from {url}"))
}

fn validate_commit_sha(sha: &str) -> Result<()> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("GitHub release tag returned an invalid commit SHA: '{sha}'");
    }
    Ok(())
}

async fn download_file(client: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    let response = http_retry::send(client.get(url), ReplaySafety::Idempotent)
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !response.status.is_success() {
        anyhow::bail!("download failed for {url}: HTTP {}", response.status);
    }
    fs::write(path, response.body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

async fn verify_checksum(client: &reqwest::Client, url: &str, archive_path: &Path) -> Result<()> {
    let response = http_retry::send(client.get(url), ReplaySafety::Idempotent)
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !response.status.is_success() {
        anyhow::bail!(
            "checksum download failed for {url}: HTTP {}",
            response.status
        );
    }
    let checksum_text = String::from_utf8(response.body)
        .with_context(|| format!("reading checksum response from {url}"))?;

    let expected = extract_checksum_digest(&checksum_text)
        .context("checksum file did not contain a SHA256 digest")?;

    let actual = {
        let bytes = fs::read(archive_path)
            .with_context(|| format!("reading downloaded asset {}", archive_path.display()))?;
        hex::encode(Sha256::digest(&bytes))
    };

    if !checksum_matches(expected, &actual) {
        anyhow::bail!(
            "SHA256 mismatch for {} (expected {}, got {})",
            archive_path.display(),
            expected,
            actual
        );
    }

    Ok(())
}

fn extract_checksum_digest(checksum_text: &str) -> Option<&str> {
    checksum_text
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
}

fn checksum_matches(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

fn extract_binary(archive_path: &Path, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let binary_name = extracted_binary_name();
    if archive_path.extension().and_then(|ext| ext.to_str()) == Some("zip") {
        extract_zip_binary(archive_path, &binary_name, output_path)?;
    } else {
        extract_tar_gz_binary(archive_path, &binary_name, output_path)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = fs::metadata(output_path)
            .with_context(|| format!("reading metadata for {}", output_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(output_path, perms)
            .with_context(|| format!("setting permissions on {}", output_path.display()))?;
    }

    Ok(())
}

fn extract_tar_gz_binary(archive_path: &Path, binary_name: &str, output_path: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening archive {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("listing tar archive entries")? {
        let mut entry = entry.context("reading tar archive entry")?;
        let path = entry.path().context("reading tar entry path")?;
        if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut out = fs::File::create(output_path)
                .with_context(|| format!("creating {}", output_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extracting {}", output_path.display()))?;
            return Ok(());
        }
    }

    anyhow::bail!(
        "binary '{}' not found inside {}",
        binary_name,
        archive_path.display()
    );
}

fn extract_zip_binary(archive_path: &Path, binary_name: &str, output_path: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening archive {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("opening zip archive")?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading zip entry #{index}"))?;
        let name = entry.name().replace('\\', "/");
        if Path::new(&name)
            .file_name()
            .and_then(|value| value.to_str())
            == Some(binary_name)
        {
            let mut out = fs::File::create(output_path)
                .with_context(|| format!("creating {}", output_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extracting {}", output_path.display()))?;
            return Ok(());
        }
    }

    anyhow::bail!(
        "binary '{}' not found inside {}",
        binary_name,
        archive_path.display()
    );
}

fn asset_name() -> String {
    if cfg!(target_os = "windows") {
        format!("cs-{}.zip", release_target())
    } else {
        format!("cs-{}.tar.gz", release_target())
    }
}

fn extracted_binary_name() -> String {
    if cfg!(target_os = "windows") {
        format!("{BIN_NAME}.exe")
    } else {
        BIN_NAME.to_string()
    }
}

fn release_target() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{platform}-{arch}")
}

fn release_tag(version: &str) -> String {
    let version = version.trim();
    // The dev channel uses the bare tag "dev", not "vdev".
    if version == "dev" {
        return "dev".to_string();
    }
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn release_api_url(version: Option<&str>) -> String {
    let base = github_api_base();

    match version {
        // Encoded for the same reason as `tag_ref_api_url`: the tag is a path
        // segment, and `url` would otherwise resolve `..` inside it and send
        // the request to a different repository.
        Some(version) => format!(
            "{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/tags/{}",
            urlencoding::encode(&release_tag(version))
        ),
        None => format!("{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest"),
    }
}

fn tag_ref_api_url(tag: &str) -> String {
    format!(
        "{}/repos/{REPO_OWNER}/{REPO_NAME}/git/ref/tags/{}",
        github_api_base(),
        urlencoding::encode(tag)
    )
}

fn git_tag_api_url(sha: &str) -> String {
    format!(
        "{}/repos/{REPO_OWNER}/{REPO_NAME}/git/tags/{sha}",
        github_api_base()
    )
}

fn github_api_base() -> String {
    std::env::var("CS_GITHUB_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

/// Normalize a `--version` argument, rejecting anything that is not a plain
/// semantic version.
///
/// The value reaches `release_api_url` as a path segment. `url` resolves `..`
/// segments per the WHATWG spec, so an unencoded traversal would walk the
/// request onto another repository's release metadata. `release_api_url` now
/// percent-encodes, which contains the value; this rejects it outright so the
/// safety of that path never rests on a downstream string comparison, and so a
/// typo is reported as a bad argument rather than as a 404.
fn validate_requested_version(version: &str) -> Result<String> {
    let normalized = normalize_version(version);
    Version::parse(&normalized).map_err(|err| {
        anyhow::anyhow!(
            "invalid --version '{version}': expected a semantic version such as 20260731.1.0 ({err}). \
             Use --dev for the rolling development build."
        )
    })?;
    Ok(normalized)
}

fn update_ttl_secs() -> i64 {
    std::env::var("CS_UPDATE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(UPDATE_TTL_SECS)
}

fn update_cache_path() -> anyhow::Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("update-check.json"))
}

fn load_update_cache() -> Option<UpdateCache> {
    let path = update_cache_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_update_cache(cache: &UpdateCache) {
    let path = match update_cache_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = fs::write(path, json);
    }
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Greater)
}

fn is_older_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Less)
}

fn is_dev_update_available(candidate: &str, current: &str) -> bool {
    if is_newer_version(candidate, current) {
        return true;
    }
    if is_dev_version(current) && is_dev_version(candidate) {
        let candidate = Version::parse(&normalize_version(candidate)).ok();
        let current = Version::parse(&normalize_version(current)).ok();
        return matches!((candidate, current), (Some(candidate), Some(current))
            if candidate.major == current.major
                && candidate.minor == current.minor
                && candidate.patch == current.patch
                && candidate.pre.as_str() == "dev"
                && current.pre.as_str().starts_with("dev."));
    }
    // Explicit --dev should be able to switch from a stable/base install to the
    // rolling dev build with the same base version, e.g. 20260712.1.0 -> 20260712.1.0-dev.
    if !is_dev_version(candidate) {
        return false;
    }
    let Some(candidate_base) = version_base(candidate) else {
        return false;
    };
    let Some(current_base) = version_base(current) else {
        return false;
    };
    candidate_base >= current_base
}

fn version_base(version: &str) -> Option<(u64, u64, u64)> {
    let parsed = match Version::parse(&normalize_version(version)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{version}': {e}");
            return None;
        }
    };
    Some((parsed.major, parsed.minor, parsed.patch))
}

fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left_parsed = match Version::parse(&normalize_version(left)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{left}': {e}");
            return None;
        }
    };
    let right_parsed = match Version::parse(&normalize_version(right)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{right}': {e}");
            return None;
        }
    };
    Some(left_parsed.cmp(&right_parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attestation_verification_pins_repository_workflow_and_release_ref() {
        let archive = Path::new("/tmp/cs-linux-amd64.tar.gz");
        let bundle = Path::new("/tmp/codex-switch-build-provenance.json");
        let source_digest = "0123456789abcdef0123456789abcdef01234567";
        let args = attestation_verify_args(archive, bundle, "v20260729.2.0", source_digest);

        assert_eq!(
            args,
            vec![
                "attestation",
                "verify",
                "/tmp/cs-linux-amd64.tar.gz",
                "--bundle",
                "/tmp/codex-switch-build-provenance.json",
                "--repo",
                "xjoker/codex-switch",
                "--signer-workflow",
                "xjoker/codex-switch/.github/workflows/release.yml",
                "--source-ref",
                "refs/tags/v20260729.2.0",
                "--source-digest",
                source_digest,
                "--deny-self-hosted-runners",
            ]
        );
    }

    #[test]
    fn version_compare_ignores_v_prefix() {
        assert!(is_newer_version("v0.0.2", "0.0.1"));
        assert!(is_older_version("0.0.1", "v0.0.2"));
    }

    #[test]
    fn calendar_versions_remain_semver_comparable() {
        assert!(Version::parse("20260712.1").is_err());
        assert!(Version::parse("20260712.1.0").is_ok());
        assert!(is_newer_version("20260712.1.0", "0.0.21"));
        assert!(is_newer_version(
            "20260712.1.0-dev.20260712000000",
            "0.0.22-dev.20260711000000"
        ));
        assert!(is_newer_version("20260712.2.0", "20260712.1.0"));
        assert!(is_newer_version("20260713.1.0", "20260712.9.0"));
        assert!(is_newer_version(
            "20260712.1.0",
            "20260712.1.0-dev.20260712000000"
        ));
        assert!(is_dev_update_available(
            "20260712.1.0-dev.20260712000000",
            "20260712.1.0"
        ));
    }

    #[test]
    fn calendar_stable_release_upgrades_every_supported_legacy_version_family() {
        let stable = "20260713.1.0";
        for current in [
            "0.0.21",
            "0.0.22-dev.20260711000000",
            "20260712.1.0-dev.20260712000000",
            "20260712.2.0-dev",
        ] {
            assert!(
                is_newer_version(stable, current),
                "{current} must be able to graduate to stable {stable}"
            );
        }
    }

    #[test]
    fn release_api_url_uses_latest_or_tag_endpoint() {
        assert_eq!(
            release_api_url(None),
            "https://api.github.com/repos/xjoker/codex-switch/releases/latest"
        );
        assert_eq!(
            release_api_url(Some("0.1.0")),
            "https://api.github.com/repos/xjoker/codex-switch/releases/tags/v0.1.0"
        );
    }

    /// `--version` is interpolated into a GitHub API path. `url` resolves `..`
    /// segments per the WHATWG spec, so an unencoded value can walk the request
    /// out of this repository and onto another one's release metadata. Sibling
    /// `tag_ref_api_url` already encodes; this closes the inconsistency rather
    /// than leaving the safety of the path to a downstream string comparison.
    #[test]
    fn release_api_url_percent_encodes_the_requested_version() {
        let url = release_api_url(Some("0.1.0/../../../../../attacker/evil/releases/latest"));

        assert!(
            !url.contains("/../"),
            "path traversal survived encoding: {url}"
        );
        assert!(
            url.starts_with("https://api.github.com/repos/xjoker/codex-switch/releases/tags/"),
            "the request must stay inside this repository: {url}"
        );
    }

    /// The encoding above keeps a hostile value inside its path segment; this
    /// rejects it outright, before any request is built, so the error names the
    /// bad input instead of surfacing as a confusing 404.
    #[test]
    fn a_requested_version_that_is_not_semver_is_rejected_before_any_request() {
        assert_eq!(
            validate_requested_version("20260731.1.0").unwrap(),
            "20260731.1.0"
        );
        assert_eq!(
            validate_requested_version("v20260731.1.0").unwrap(),
            "20260731.1.0"
        );

        let err = validate_requested_version("0.1.0/../../../../../attacker/evil/releases/latest")
            .unwrap_err();
        assert!(err.to_string().contains("invalid --version"), "{err}");

        // The dev channel is reached with `--dev`, not by naming a tag: this
        // has never resolved, and now says so instead of 404-ing.
        assert!(validate_requested_version("dev").is_err());
    }

    #[test]
    fn release_tag_dev_has_no_v_prefix() {
        assert_eq!(release_tag("dev"), "dev");
        assert_eq!(release_tag("0.1.0"), "v0.1.0");
        assert_eq!(release_tag("v0.1.0"), "v0.1.0");
    }

    #[test]
    fn release_api_url_dev_uses_dev_tag() {
        assert_eq!(
            release_api_url(Some("dev")),
            "https://api.github.com/repos/xjoker/codex-switch/releases/tags/dev"
        );
    }

    #[test]
    fn tag_ref_api_url_uses_the_exact_release_tag() {
        assert_eq!(
            tag_ref_api_url("dev"),
            "https://api.github.com/repos/xjoker/codex-switch/git/ref/tags/dev"
        );
        assert_eq!(
            tag_ref_api_url("release/candidate"),
            "https://api.github.com/repos/xjoker/codex-switch/git/ref/tags/release%2Fcandidate"
        );
    }

    #[test]
    fn commit_digest_must_be_a_full_sha1() {
        validate_commit_sha("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(validate_commit_sha("deadbeef").is_err());
        assert!(validate_commit_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn is_dev_version_detects_prerelease() {
        assert!(is_dev_version("1.2.3-dev"));
        assert!(is_dev_version("1.2.3-dev.20260408143000"));
        assert!(is_dev_version("1.2.3-dev+abc1234"));
        assert!(!is_dev_version("1.2.3"));
    }

    #[test]
    fn dev_update_can_switch_from_same_base_stable() {
        assert!(is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20"
        ));
        assert!(is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20-dev.20260701090000"
        ));
        assert!(!is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20-dev.20260701094804"
        ));
        assert!(!is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.21"
        ));
    }

    #[test]
    fn short_dev_version_replaces_legacy_timestamped_dev_on_the_same_base() {
        assert!(is_dev_update_available(
            "20260712.1.0-dev",
            "20260712.1.0-dev.20260712055522"
        ));
        assert!(!is_dev_update_available(
            "20260712.1.0-dev",
            "20260712.1.0-dev"
        ));
    }

    #[test]
    fn homebrew_dev_hint_avoids_removed_binary_and_unreviewed_pipe_command() {
        let hint = super::homebrew_dev_install_hint();
        assert!(hint.contains("brew uninstall codex-switch"));
        assert!(hint.contains(
            "github.com/xjoker/codex-switch/wiki/Development-Releases#install-the-rolling-dev-build"
        ));
        assert!(
            !hint.contains("blob/master/"),
            "hint must not point at repository files on master; the published Wiki follows dev"
        );
        assert!(!hint.contains("| bash"));
        assert!(!hint.contains("self-update"));
    }

    #[test]
    fn homebrew_dev_error_wraps_the_install_hint_once() {
        let message = super::homebrew_dev_install_error();
        assert!(message.contains("To switch to dev, run `brew uninstall codex-switch`"));
        assert!(!message.contains("run `run `"));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preflight_rejects_a_read_only_install_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir(&install_dir).unwrap();
        let executable = install_dir.join("codex-switch");
        fs::write(&executable, b"old binary").unwrap();
        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let error = ensure_replace_parent_writable(&executable, UpdatePlatform::Unix, "v1.2.3")
            .unwrap_err();

        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.to_string().contains("not writable"));
        assert!(error.to_string().contains("user-level installer"));
    }

    #[test]
    fn unix_system_install_hint_separates_legacy_migration_from_explicit_system_updates() {
        let hint = replacement_permission_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            "dev",
        );

        assert!(hint.contains("legacy direct install"));
        assert!(hint.contains("releases/download/dev/install.sh"));
        assert!(hint.contains("--dev"));
        assert!(hint.contains("intentionally installed with `--system`"));
        assert!(hint.contains("sudo codex-switch self-update"));
    }

    #[test]
    fn unix_user_install_hint_never_recommends_sudo() {
        let hint = replacement_permission_hint(
            Path::new("/home/alice/.local/bin/codex-switch"),
            UpdatePlatform::Unix,
            "v20260713.3.0",
        );

        assert!(hint.contains("user-owned install directory"));
        assert!(hint.contains("reinstall"));
        assert!(!hint.contains("sudo"));
    }

    #[test]
    fn windows_user_install_hint_never_recommends_administrator() {
        let hint = replacement_permission_hint(
            Path::new(r"C:\Users\Alice\AppData\Local\Programs\codex-switch\codex-switch.exe"),
            UpdatePlatform::Windows,
            "v20260713.3.0",
        );

        assert!(hint.contains("close running codex-switch processes"));
        assert!(hint.contains("user-level installer"));
        assert!(!hint.contains("Administrator"));
        assert!(!hint.contains("sudo"));
    }

    #[test]
    fn migration_hint_does_not_embed_an_untrusted_release_tag() {
        let hint = replacement_permission_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            "v1.2.3;echo-pwned",
        );

        assert!(hint.contains("releases/latest/download/install.sh"));
        assert!(!hint.contains("echo-pwned"));
    }

    #[test]
    fn markerless_unix_system_install_requires_the_dev_installer() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            false,
            true,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("One-time setup required"));
        assert!(hint.contains("releases/download/dev/install.sh"));
        assert!(hint.contains("bash -s -- --dev"));
        assert!(hint.contains("--dev --system"));
    }

    #[test]
    fn legacy_migration_message_is_actionable_without_internal_jargon() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            false,
            true,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.starts_with("One-time setup required"));
        assert!(hint.contains("Recommended"));
        assert!(hint.contains("Future updates will not need sudo"));
        assert!(hint.contains("Profiles and configuration are preserved"));
        assert!(hint.contains("Keep the system-wide install instead"));
        assert!(hint.contains('\n'));
        assert!(!hint.contains("legacy direct install detected"));
        assert!(!hint.contains("direct self-update is paused"));
    }

    #[test]
    fn markerless_unix_system_install_requires_the_stable_installer() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            false,
            false,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("releases/latest/download/install.sh"));
        assert!(!hint.contains("--dev"));
        assert!(hint.contains("--system"));
    }

    #[test]
    fn marked_system_and_user_installs_do_not_enter_legacy_migration() {
        assert!(
            legacy_system_install_migration_hint(
                Path::new("/usr/local/bin/codex-switch"),
                UpdatePlatform::Unix,
                true,
                false,
                None,
            )
            .is_none()
        );
        assert!(
            legacy_system_install_migration_hint(
                Path::new("/home/alice/.local/bin/codex-switch"),
                UpdatePlatform::Unix,
                false,
                false,
                None,
            )
            .is_none()
        );
        assert!(
            legacy_system_install_migration_hint(
                Path::new(r"C:\Users\Alice\AppData\Local\Programs\codex-switch\codex-switch.exe"),
                UpdatePlatform::Windows,
                false,
                false,
                None,
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn homebrew_symlink_is_resolved_before_legacy_migration_check() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create temp directory");
        let cellar_dir = temp.path().join("Cellar/codex-switch/20260713.4.0/bin");
        fs::create_dir_all(&cellar_dir).expect("create fake Cellar path");
        let cellar_binary = cellar_dir.join("codex-switch");
        fs::write(&cellar_binary, b"fixture").expect("write fake Homebrew binary");
        let symlink_path = temp.path().join("codex-switch");
        symlink(&cellar_binary, &symlink_path).expect("create Homebrew symlink");

        let resolved = canonical_executable_path(symlink_path);
        assert!(resolved.to_string_lossy().contains("/Cellar/codex-switch/"));
        assert!(
            legacy_system_install_migration_hint(
                &resolved,
                UpdatePlatform::Unix,
                false,
                false,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn markerless_system_install_preserves_an_exact_requested_version() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch"),
            UpdatePlatform::Unix,
            false,
            false,
            Some("v20260712.2.0"),
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("releases/download/v20260712.2.0/install.sh"));
        assert!(hint.contains("| CS_VERSION=20260712.2.0 bash"));
        assert!(!hint.contains("releases/latest/download"));
    }

    #[test]
    fn checksum_matches_lowercase_expected() {
        assert!(checksum_matches(
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2"
        ));
    }

    #[test]
    fn checksum_matches_uppercase_expected() {
        assert!(checksum_matches(
            "D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2",
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2"
        ));
    }

    #[test]
    fn checksum_matches_rejects_mismatch() {
        assert!(!checksum_matches(
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn checksum_digest_extracts_gnu_two_column_format() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!("{digest}  cs-darwin-arm64.tar.gz\n");

        assert_eq!(extract_checksum_digest(&text), Some(digest));
        assert!(checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            digest
        ));
    }

    #[test]
    fn checksum_digest_matches_uppercase_hash() {
        let lowercase = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let uppercase = lowercase.to_ascii_uppercase();
        let text = format!("{uppercase}  archive.tar.gz\n");

        assert!(checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            lowercase
        ));
    }

    #[test]
    fn checksum_digest_rejects_wrong_hash() {
        let actual = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let text = format!("{wrong}  archive.tar.gz\n");

        assert!(!checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            actual
        ));
    }

    #[test]
    fn checksum_digest_rejects_empty_or_whitespace_only_files() {
        assert_eq!(extract_checksum_digest(""), None);
        assert_eq!(extract_checksum_digest(" \t\n"), None);
    }
}
