use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPO_OWNER: &str = "xjoker";
const REPO_NAME: &str = "codex-switch";
const BIN_NAME: &str = "codex-switch";
const UPDATE_TTL_SECS: i64 = 12 * 60 * 60;

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
    assets: Vec<GithubAsset>,
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

pub async fn self_update(version: Option<&str>, show_progress: bool) -> Result<SelfUpdateResult> {
    let install_source = detect_install_source();
    if install_source == InstallSource::Homebrew {
        anyhow::bail!(
            "Homebrew-managed install detected. Run `{}` instead.",
            install_source.upgrade_hint()
        );
    }

    let current_version = current_version().to_string();
    let release = fetch_release(version).await?;
    let latest_version = normalize_version(&release.tag_name);

    if let Some(requested) = version {
        let requested = normalize_version(requested);
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

    let client =
        crate::auth::build_http_client().context("building HTTP client for self-update")?;
    let archive_name = asset_name();
    let archive_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .cloned()
        .with_context(|| format!("release does not contain asset '{archive_name}'"))?;
    let checksum_name = format!("{archive_name}.sha256");
    let checksum_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .cloned()
        .with_context(|| format!("release does not contain checksum asset '{checksum_name}'"))?;

    let temp_dir = tempfile::tempdir().context("creating temporary update directory")?;
    let archive_path = temp_dir.path().join(&archive_asset.name);
    if show_progress {
        eprintln!("Downloading {}...", archive_asset.name);
    }
    download_file(&client, &archive_asset.browser_download_url, &archive_path).await?;
    verify_checksum(&client, &checksum_asset.browser_download_url, &archive_path).await?;

    let extracted_path = temp_dir.path().join(extracted_binary_name());
    if show_progress {
        eprintln!("Extracting update package...");
    }
    extract_binary(&archive_path, &extracted_path)?;

    if show_progress {
        eprintln!("Replacing current executable...");
    }
    self_replace::self_replace(&extracted_path).context("replacing current executable")?;

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
    let client =
        crate::auth::build_http_client().context("building HTTP client for update check")?;
    let url = release_api_url(version);
    let release = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("requesting GitHub release metadata")?
        .error_for_status()
        .context("GitHub release request failed")?
        .json::<GithubRelease>()
        .await
        .context("parsing GitHub release metadata")?;
    Ok(release)
}

async fn download_file(client: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading response body from {url}"))?;
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

async fn verify_checksum(client: &reqwest::Client, url: &str, archive_path: &Path) -> Result<()> {
    let checksum_text = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("checksum download failed for {url}"))?
        .text()
        .await
        .with_context(|| format!("reading checksum response from {url}"))?;

    let expected = checksum_text
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .context("checksum file did not contain a SHA256 digest")?;

    let actual = {
        let bytes = fs::read(archive_path)
            .with_context(|| format!("reading downloaded asset {}", archive_path.display()))?;
        hex::encode(Sha256::digest(&bytes))
    };

    if actual != expected {
        anyhow::bail!(
            "SHA256 mismatch for {} (expected {}, got {})",
            archive_path.display(),
            expected,
            actual
        );
    }

    Ok(())
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
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn release_api_url(version: Option<&str>) -> String {
    let base = std::env::var("CS_GITHUB_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string());

    match version {
        Some(version) => format!(
            "{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/tags/{}",
            release_tag(version)
        ),
        None => format!("{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest"),
    }
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

fn update_ttl_secs() -> i64 {
    std::env::var("CS_UPDATE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(UPDATE_TTL_SECS)
}

fn update_cache_path() -> PathBuf {
    crate::auth::app_home().join("update-check.json")
}

fn load_update_cache() -> Option<UpdateCache> {
    let path = update_cache_path();
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_update_cache(cache: &UpdateCache) {
    let path = update_cache_path();
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

fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left = Version::parse(&normalize_version(left)).ok()?;
    let right = Version::parse(&normalize_version(right)).ok()?;
    Some(left.cmp(&right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_ignores_v_prefix() {
        assert!(is_newer_version("v0.0.2", "0.0.1"));
        assert!(is_older_version("0.0.1", "v0.0.2"));
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
}
