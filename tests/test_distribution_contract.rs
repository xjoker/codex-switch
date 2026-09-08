use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_file(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    normalize_line_endings(&text)
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn assert_before(text: &str, first: &str, second: &str) {
    let first_pos = text
        .find(first)
        .unwrap_or_else(|| panic!("missing required content: {first}"));
    let second_pos = text
        .find(second)
        .unwrap_or_else(|| panic!("missing required content: {second}"));
    assert!(
        first_pos < second_pos,
        "expected `{first}` to appear before `{second}`"
    );
}

fn markdown_links(text: &str) -> Vec<&str> {
    let mut links = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("](") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find(')') else {
            break;
        };
        links.push(remaining[..end].trim());
        remaining = &remaining[end + 1..];
    }
    links
}

fn github_heading_slug(heading: &str) -> String {
    heading
        .trim()
        .trim_end_matches('#')
        .trim()
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn assert_markdown_anchor_exists(path: &Path, anchor: &str) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    assert!(
        text.lines()
            .filter_map(|line| line.strip_prefix('#'))
            .any(|line| {
                let heading = line.trim_start_matches('#').trim();
                github_heading_slug(heading) == anchor
            }),
        "missing anchor `#{anchor}` in {}",
        path.display()
    );
}

#[test]
fn version_file_is_the_release_source_of_truth() {
    let version = repo_file("VERSION").trim().to_string();
    let manifest = repo_file("Cargo.toml");
    let release = repo_file(".github/workflows/release.yml");

    // Pin the documented YYYYMMDD.N.0 shape rather than one literal version, which
    // every release had to edit. This also catches the two forms RELEASE.md warns
    // about: the two-component `20260712.1`, and YYYYDDMM, which sorts wrongly.
    let (date, rest) = version.split_once('.').expect("version needs a date part");
    let (sequence, patch) = rest.split_once('.').expect("version must be YYYYMMDD.N.0");
    assert!(
        date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()),
        "version must start with an 8-digit YYYYMMDD date, got {date:?}"
    );
    let month: u32 = date[4..6].parse().expect("month must be numeric");
    let day: u32 = date[6..8].parse().expect("day must be numeric");
    assert!(
        (1..=12).contains(&month) && (1..=31).contains(&day),
        "version date must be YYYYMMDD, got month {month} day {day} in {date:?}"
    );
    assert!(
        !sequence.is_empty()
            && sequence.chars().all(|c| c.is_ascii_digit())
            && !sequence.starts_with('0'),
        "release sequence must be a positive integer starting at 1, got {sequence:?}"
    );
    assert_eq!(patch, "0", "the third component is always 0 for SemVer");
    assert!(manifest.contains(&format!("version = \"{version}\"")));
    assert!(release.contains("BASE=$(cat VERSION)"));
    assert!(!release.contains("BASE=$(grep '^version' Cargo.toml"));
}

#[test]
fn release_docs_preserve_zero_drift_across_calendar_days() {
    let release = repo_file("docs/RELEASE.md");
    let updating = repo_file("docs/wiki/Updating.md");
    let readme_cn = repo_file("README_CN.md");

    for required in [
        "`YYYYMMDD` is the version-allocation date",
        "A stable promotion may happen on a later calendar date",
        "Do not bump or edit `VERSION`, `Cargo.toml`, or `docs/CHANGELOG.md` after acceptance",
    ] {
        assert!(
            release.contains(required),
            "release docs must preserve the cross-day zero-drift contract: `{required}`"
        );
    }
    assert!(
        updating.contains("version-allocation date"),
        "user update docs must not promise that a cross-day stable tag date is encoded"
    );
    assert!(
        readme_cn.contains("版本分配日期"),
        "the Chinese README must describe the calendar component as the allocation date"
    );
}

#[test]
fn stable_release_docs_use_full_branch_refspecs() {
    let release = repo_file("docs/RELEASE.md");

    assert!(release.contains("git push origin refs/heads/master:refs/heads/master"));
    assert!(
        !release.contains("git push origin master"),
        "stable release instructions must not contradict the full-refspec rule"
    );
}

#[test]
fn ci_covers_dev_and_all_supported_hosts() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for required in [
        "push:",
        "pull_request:",
        "workflow_dispatch:",
        "dev",
        "master",
        "ubuntu-latest",
        "macos-latest",
        "windows-latest",
    ] {
        assert!(
            workflow.contains(required),
            "CI workflow must contain `{required}`"
        );
    }
}

#[test]
fn ci_runs_build_test_lint_format_audit_and_script_parsers() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for command in [
        "cargo test --all",
        "cargo clippy --all-targets -- -D warnings",
        "cargo build",
        "cargo fmt --check",
        "cargo audit",
        "bash -n scripts/install.sh",
    ] {
        assert!(
            workflow.contains(command),
            "CI workflow must execute `{command}`"
        );
    }
    assert!(
        workflow.contains("Parser]::ParseFile") && workflow.contains("scripts/install.ps1"),
        "Windows CI must parse install.ps1 with the PowerShell parser"
    );
}

#[test]
fn ci_actions_are_pinned_to_full_commit_shas() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for line in workflow
        .lines()
        .filter(|line| line.trim().starts_with("uses:"))
    {
        let reference = line
            .split_once('@')
            .map(|(_, reference)| reference.split_whitespace().next().unwrap_or(""))
            .unwrap_or("");
        assert!(
            reference.len() == 40 && reference.chars().all(|ch| ch.is_ascii_hexdigit()),
            "CI action must be pinned to a full commit SHA: {line}"
        );
    }
}

#[test]
fn self_update_provenance_requirement_is_documented() {
    let readme = repo_file("README.md");
    let readme_cn = repo_file("README_CN.md");
    let updating = repo_file("docs/wiki/Updating.md");
    let release = repo_file("docs/RELEASE.md");

    assert!(readme.contains("gh attestation verify"));
    assert!(readme_cn.contains("gh attestation verify"));
    assert!(updating.contains("codex-switch-build-provenance.json"));
    assert!(updating.contains("gh attestation verify"));
    assert!(release.contains("codex-switch-build-provenance.json"));
}

#[test]
fn wiki_sync_uses_the_reviewed_repository_sources() {
    let workflow = repo_file(".github/workflows/wiki.yml");

    for required in [
        "workflow_dispatch:",
        "branches: [dev]",
        "docs/wiki/**",
        "permissions:",
        "contents: write",
        "${{ secrets.GITHUB_TOKEN }}",
        "${GITHUB_REPOSITORY}.wiki.git",
        "refs/heads/dev:refs/remotes/origin/dev",
        "git diff --quiet \"$GITHUB_SHA\" refs/remotes/origin/dev -- docs/wiki .github/workflows/wiki.yml",
        "for page in docs/wiki/*.md; do",
        "[[ -f \"$page\" && ! -L \"$page\" ]]",
        "cp -- \"$page\" \"$wiki_dir\"/",
        "push origin HEAD:master",
    ] {
        assert!(
            workflow.contains(required),
            "Wiki workflow must contain `{required}`"
        );
    }
    assert!(!workflow.contains("WIKI_TOKEN"));
    assert!(!workflow.contains("pull_request:"));
    assert!(!workflow.contains("branches: [dev, master]"));
}

#[test]
fn wiki_links_resolve_to_reviewed_pages_and_sources() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wiki_dir = root.join("docs/wiki");
    let page_slugs: BTreeSet<String> = fs::read_dir(&wiki_dir)
        .expect("failed to list Wiki sources")
        .map(|entry| entry.expect("failed to read Wiki source entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .filter_map(|path| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .collect();
    let repository_prefix = "https://github.com/xjoker/codex-switch/";

    for entry in fs::read_dir(&wiki_dir).expect("failed to list Wiki sources") {
        let path = entry.expect("failed to read Wiki source entry").path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        for link in markdown_links(&text) {
            if let Some(target) = link.strip_prefix(repository_prefix) {
                let Some(target) = target
                    .strip_prefix("blob/dev/")
                    .or_else(|| target.strip_prefix("tree/dev/"))
                else {
                    if target.starts_with("blob/") || target.starts_with("tree/") {
                        panic!(
                            "{} links to an unreviewed repository branch: {link}",
                            path.display()
                        );
                    }
                    continue;
                };
                let (target_path, anchor) = target
                    .split_once('#')
                    .map_or((target, None), |(file, anchor)| (file, Some(anchor)));
                let local_path = root.join(target_path);
                assert!(
                    local_path.exists(),
                    "{} links to missing repository source: {link}",
                    path.display()
                );
                if let Some(anchor) = anchor {
                    assert_markdown_anchor_exists(&local_path, anchor);
                }
                continue;
            }
            if let Some(anchor) = link.strip_prefix('#') {
                assert_markdown_anchor_exists(&path, anchor);
                continue;
            }
            if link.contains("://") {
                continue;
            }

            let (page, anchor) = link
                .split_once('#')
                .map_or((link, None), |(page, anchor)| (page, Some(anchor)));
            let slug = page.trim_end_matches(".md");
            assert!(
                page_slugs.contains(slug),
                "{} links to missing Wiki page: {link}",
                path.display()
            );
            if let Some(anchor) = anchor {
                assert_markdown_anchor_exists(&wiki_dir.join(format!("{slug}.md")), anchor);
            }
        }
    }
}

#[test]
fn release_rejects_shell_metacharacters_in_tags_and_uses_env_in_scripts() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("TAG_PATTERN="));
    assert!(workflow.contains("BASE_PATTERN=\"${BASE//./\\\\.}\""));
    assert!(workflow.contains("[[ ! \"$TAG\" =~ $TAG_PATTERN ]]"));
    assert!(!workflow.contains("VERSION=\"${{ needs.meta.outputs.version }}\""));
    assert!(!workflow.contains("${{ github.ref }}` at ${{ github.sha }}"));
    assert!(workflow.contains("RELEASE_VERSION: ${{ needs.meta.outputs.version }}"));
    assert!(workflow.contains("persist-credentials: false"));
}

#[test]
fn unix_installer_verifies_checksum_before_extracting() {
    let script = repo_file("scripts/install.sh");

    assert!(script.contains("${DOWNLOAD_URL}.sha256"));
    assert!(script.contains("EXPECTED_SHA256"));
    assert!(script.contains("sha256sum") && script.contains("shasum -a 256"));
    assert_before(&script, "EXPECTED_SHA256", "tar xzf");
    for required in [
        "USER_INSTALL_DIR=\"${HOME}/.local/bin\"",
        "SYSTEM_INSTALL_DIR=\"/usr/local/bin\"",
        "--system",
        "LEGACY_BIN",
        "install -m 0755",
        "sudo install -m 0755",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must contain `{required}`"
        );
    }
}

#[test]
fn unix_installer_preserves_migration_and_path_lifecycle() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "*/fish)",
        "PROFILE_FILE=\"${HOME}/.config/fish/config.fish\"",
        "# >>> codex-switch PATH >>>",
        "# <<< codex-switch PATH <<<",
        "remove_managed_path_blocks",
        "remove_path_block \"${HOME}/.zprofile\"",
        "remove_path_block \"${HOME}/.bash_profile\"",
        "remove_path_block \"${HOME}/.profile\"",
        "remove_path_block \"${HOME}/.config/fish/config.fish\"",
        "!seen_begin || !seen_end || inside",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must contain `{required}`"
        );
    }

    assert_before(&script, "tar xzf", "sudo -v");
    assert_before(
        &script,
        "mkdir -p \"$INSTALL_DIR\"",
        "sudo rm -f \"$LEGACY_BIN\"",
    );
    assert!(script.contains(
        "if [ \"$SYSTEM_INSTALL\" = false ]; then\n    remove_managed_path_blocks\n  fi"
    ));
}

#[test]
fn unix_installer_rewrites_shell_profiles_atomically() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "remove_path_block() (",
        "resolve_profile_target() (",
        "file_identity() (",
        "while [ -L \"$profile_target\" ]",
        "link_target=\"$(readlink \"$profile_target\")\"",
        "cd -P \"$(dirname \"$profile_target\")\" && pwd -P",
        "mktemp \"${profile_dir}/.${BINARY_NAME}.XXXXXX\"",
        "cp -p \"$profile_target\" \"$tmp_file\"",
        "current_profile_target=\"$(resolve_profile_target \"$profile_file\")\"",
        "current_profile_identity=\"$(file_identity \"$current_profile_target\")\"",
        "mv -f \"$tmp_file\" \"$profile_target\"",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve the atomic profile rewrite step `{required}`"
        );
    }
    assert!(
        !script.contains("cat \"$tmp_file\" > \"$profile_file\""),
        "Unix installer must not truncate a live shell profile in place"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_preserves_multi_level_profile_symlinks() {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nremove_managed_path_blocks() {")
        .next()
        .expect("installer must define remove_managed_path_blocks");
    let temp = tempfile::tempdir().unwrap();
    let real_profile = temp.path().join("real-profile");
    let middle_link = temp.path().join("middle-profile");
    let profile_link = temp.path().join(".zprofile");
    fs::write(
        &real_profile,
        "export KEEP=1\n# >>> codex-switch PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch PATH <<<\n",
    )
    .unwrap();
    symlink("real-profile", &middle_link).unwrap();
    symlink("middle-profile", &profile_link).unwrap();

    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
    )
    .unwrap();
    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile_link)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "remove_path_block failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::symlink_metadata(&profile_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(&middle_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real_profile).unwrap(),
        "export KEEP=1\n"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_symlink_changes_during_rewrite() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nremove_managed_path_blocks() {")
        .next()
        .expect("installer must define remove_managed_path_blocks");
    let temp = tempfile::tempdir().unwrap();
    let original_profile = temp.path().join("original-profile");
    let replacement_profile = temp.path().join("replacement-profile");
    let profile_link = temp.path().join(".zprofile");
    let managed =
        "# >>> codex-switch PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch PATH <<<\n";
    let original_contents = format!("export ORIGINAL=1\n{managed}");
    let replacement_contents = format!("export REPLACEMENT=1\n{managed}");
    fs::write(&original_profile, &original_contents).unwrap();
    fs::write(&replacement_profile, &replacement_contents).unwrap();
    symlink("original-profile", &profile_link).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nrm -f \"$PROFILE_LINK\"\nln -s \"$REPLACEMENT_PROFILE\" \"$PROFILE_LINK\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();

    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
    )
    .unwrap();
    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile_link)
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("PROFILE_LINK", &profile_link)
        .env("REPLACEMENT_PROFILE", &replacement_profile)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a changed profile symlink must abort the rewrite"
    );
    assert_eq!(
        fs::read_to_string(&original_profile).unwrap(),
        original_contents
    );
    assert_eq!(
        fs::read_to_string(&replacement_profile).unwrap(),
        replacement_contents
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_parent_symlink_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nremove_managed_path_blocks() {")
        .next()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let dir_a = temp.path().join("dir-a");
    let dir_b = temp.path().join("dir-b");
    let current = temp.path().join("current");
    fs::create_dir(&dir_a).unwrap();
    fs::create_dir(&dir_b).unwrap();
    let managed =
        "# >>> codex-switch PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch PATH <<<\n";
    let contents_a = format!("export A=1\n{managed}");
    let contents_b = format!("export B=1\n{managed}");
    fs::write(dir_a.join("profile"), &contents_a).unwrap();
    fs::write(dir_b.join("profile"), &contents_b).unwrap();
    symlink("dir-a", &current).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nrm -f \"$CURRENT_LINK\"\nln -s \"$NEW_DIR\" \"$CURRENT_LINK\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
    )
    .unwrap();

    let output = Command::new("bash")
        .arg(&harness)
        .arg(current.join("profile"))
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("CURRENT_LINK", &current)
        .env("NEW_DIR", &dir_b)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir_a.join("profile")).unwrap(),
        contents_a
    );
    assert_eq!(
        fs::read_to_string(dir_b.join("profile")).unwrap(),
        contents_b
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_inode_changes() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nremove_managed_path_blocks() {")
        .next()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    let replacement = temp.path().join("replacement");
    let managed =
        "# >>> codex-switch PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch PATH <<<\n";
    fs::write(&profile, format!("export OLD=1\n{managed}")).unwrap();
    let replacement_contents = format!("export NEW=1\n{managed}");
    fs::write(&replacement, &replacement_contents).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nmv -f \"$REPLACEMENT_PROFILE\" \"$PROFILE_FILE\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
    )
    .unwrap();

    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile)
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("PROFILE_FILE", &profile)
        .env("REPLACEMENT_PROFILE", &replacement)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&profile).unwrap(), replacement_contents);
}

#[test]
fn release_build_installs_cross_with_locked_dependencies() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains(
        "cargo install cross --locked --git https://github.com/cross-rs/cross --rev 64b5bb4d3d34de062552b9a2093affe77b4ad16a"
    ));
}

#[test]
fn unix_installer_records_and_cleans_explicit_system_install_intent() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "SYSTEM_INSTALL_MARKER",
        ".codex-switch-system-install-v1",
        "install -m 0644 /dev/null \"$SYSTEM_INSTALL_MARKER\"",
        "sudo install -m 0644 /dev/null \"$SYSTEM_INSTALL_MARKER\"",
        "rm -f \"$LEGACY_BIN\" \"$SYSTEM_INSTALL_MARKER\"",
        "sudo rm -f \"$LEGACY_BIN\" \"$SYSTEM_INSTALL_MARKER\"",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve system-install marker lifecycle: `{required}`"
        );
    }
}

#[test]
fn windows_installer_verifies_checksum_before_extracting() {
    let script = repo_file("scripts/install.ps1");

    assert!(script.contains("$ChecksumUrl"));
    assert!(script.contains("Get-FileHash"));
    assert!(script.contains("SHA256"));
    assert_before(&script, "Get-FileHash", "Expand-Archive");
    assert!(
        script.contains("Checksum mismatch"),
        "Windows installer must fail clearly on checksum mismatch"
    );
    assert!(script.contains("$env:LOCALAPPDATA"));
    assert!(script.contains("SetEnvironmentVariable(\"Path\", $NewPath, \"User\")"));
}

#[test]
fn self_update_checks_replace_permission_before_archive_download() {
    let update = repo_file("src/update.rs");

    assert_before(
        &update,
        "ensure_replace_parent_writable(&executable, platform, &release.tag_name)?",
        "download_file(&client, &archive_asset.browser_download_url",
    );
    assert!(!update.contains("permission denied? try: sudo codex-switch self-update"));
    assert!(!update.contains("retry from PowerShell as Administrator"));
}

#[test]
fn self_update_attestation_is_bound_to_the_current_tag_commit() {
    let update = repo_file("src/update.rs");

    assert!(update.contains("\"--source-digest\""));
    assert!(update.contains("fetch_tag_commit_sha(&client, &release.tag_name).await?"));
    assert!(update.contains("if confirmed_digest != source_digest"));
    assert_before(
        &update,
        "verify_build_provenance(",
        "if confirmed_digest != source_digest",
    );
}

#[test]
fn self_update_gates_markerless_system_installs_before_network_checks() {
    let command = repo_file("src/commands/update.rs");

    assert_before(
        &command,
        "ensure_legacy_system_install_migrated(use_dev, version)",
        "if check",
    );
}

#[test]
fn release_notes_prompt_legacy_users_to_run_the_matching_installer() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("Older macOS/Linux direct install"));
    assert!(workflow.contains("releases/download/dev/install.sh | bash -s -- --dev"));
    assert!(
        workflow.contains("releases/download/v${{ needs.meta.outputs.version }}/install.sh | bash")
    );
}

#[test]
fn release_verifies_archives_before_creating_a_release() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("permissions:\n  contents: read"));
    assert!(workflow.contains("release:\n") && workflow.contains("contents: write"));
    for archive in [
        "cs-linux-amd64.tar.gz",
        "cs-linux-arm64.tar.gz",
        "cs-darwin-amd64.tar.gz",
        "cs-darwin-arm64.tar.gz",
        "cs-windows-amd64.zip",
        "cs-windows-arm64.zip",
    ] {
        assert!(
            workflow.contains(archive),
            "release verification must require `{archive}`"
        );
    }
    assert!(workflow.contains("sha256sum --check"));
    assert_before(
        &workflow,
        "Verify release checksums",
        "Create GitHub Release (dev)",
    );
    assert_before(
        &workflow,
        "Verify release checksums",
        "Create GitHub Release (stable)",
    );
}

#[test]
fn release_attests_archives_before_publishing_them() {
    let workflow = repo_file(".github/workflows/release.yml");

    for permission in [
        "id-token: write",
        "attestations: write",
        "artifact-metadata: write",
    ] {
        assert!(
            workflow.contains(permission),
            "release workflow must grant `{permission}` to the attestation step"
        );
    }
    assert!(workflow.contains("actions/attest@"));
    assert!(workflow.contains("subject-path:"));
    assert!(workflow.contains("artifacts/*.tar.gz"));
    assert!(workflow.contains("artifacts/*.zip"));
    assert!(workflow.contains("codex-switch-build-provenance.json"));
    assert_eq!(
        workflow
            .matches("target_commitish: ${{ github.sha }}")
            .count(),
        2,
        "dev and stable release metadata must both record the source commit"
    );
    assert_before(
        &workflow,
        "Attest release archives",
        "Create GitHub Release (dev)",
    );
    assert_before(
        &workflow,
        "Attest release archives",
        "Create GitHub Release (stable)",
    );
}

#[test]
fn release_retests_v0019_upgrade_on_all_supported_hosts() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "legacy-upgrade:",
        "needs: [meta, build]",
        "if: needs.meta.outputs.is_dev == 'true' || needs.meta.outputs.prerelease == 'false'",
        "ubuntu-latest",
        "macos-26",
        "windows-latest",
        "Download build artifacts",
        "scripts/prepare-release-metadata.py",
        "--base-url \"http://127.0.0.1:8765/artifacts\"",
        "releases/download/v0.0.19",
        "self-update --dev",
        "self-update --stable",
        "@(& $bin --version)[0]",
        "3589fdac3d480aea83ab61dd4fb0a7592c018a44415842083df2d5f1d0bb0d2f",
        "5db981cc5f1380f3bf9ac2d66484c0cc67d06712e617c2cd0457e3058dcb12b0",
        "cbc4229285c5e8ea02c9463b7868cd03f2021f5125f5b187ff99e3bebe16e278",
        "2e365dc8273c04ee634d593eeada338a46ba7c7db2a1ca8f5c2aff30aec57a98",
        "9ff8a3f8794517771ef6959632417fdf1dfbc85b7fdf4c344998223985582e63",
    ] {
        assert!(
            workflow.contains(required),
            "Release workflow must test legacy upgrade contract: `{required}`"
        );
    }
}

#[test]
fn legacy_upgrade_uses_build_artifact_release_metadata_before_publishing() {
    let workflow = repo_file(".github/workflows/release.yml");
    let legacy = workflow
        .split_once("  legacy-upgrade:\n")
        .and_then(|(_, rest)| rest.split_once("  homebrew:\n"))
        .map(|(job, _)| job)
        .expect("release workflow must contain a bounded legacy-upgrade job");

    for required in [
        "actions/download-artifact@",
        "scripts/prepare-release-metadata.py",
        "--artifacts-dir",
        "--base-url \"http://127.0.0.1:8765/artifacts\"",
        "CS_GITHUB_API_URL=http://127.0.0.1:8765",
        "python3 -m http.server 8765 --bind 127.0.0.1",
        "\"-m\", \"http.server\", \"8765\", \"--bind\", \"127.0.0.1\"",
        "macos-26",
    ] {
        assert!(
            workflow.contains(required),
            "legacy upgrade must serve build artifacts locally: `{required}`"
        );
    }

    assert!(!legacy.contains("gh api"));
    assert!(!legacy.contains("GH_TOKEN"));
    assert!(!legacy.contains("releases/download/v${{ needs.meta.outputs.version }}"));
    assert!(legacy.contains("releases/download/v0.0.19"));
    assert!(workflow.contains("release:\n    needs: [meta, build, legacy-upgrade]"));
    assert!(workflow.contains("always() && needs.meta.result == 'success'"));
    assert!(workflow.contains("needs['legacy-upgrade'].result == 'success'"));
    assert!(workflow.contains("homebrew:\n    needs: [meta, release, legacy-upgrade]"));
    assert!(workflow.contains("overwrite_files: true"));

    assert!(
        !workflow.contains("os: [ubuntu-latest, macos-latest, windows-latest]"),
        "legacy upgrade must pin the macOS 26 runner during the latest-label migration"
    );
}

#[test]
fn homebrew_hashes_come_from_verified_build_artifacts() {
    let workflow = repo_file(".github/workflows/release.yml");
    let homebrew = workflow
        .split_once("  homebrew:\n")
        .map(|(_, job)| job)
        .expect("release workflow must contain the Homebrew job");

    for required in [
        "needs: [meta, release, legacy-upgrade]",
        "actions/download-artifact@",
        "Verify Homebrew archive checksums",
        r#"expected=$(awk 'NF >= 1 { print $1; exit }' "$checksum")"#,
        r#"[[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]]"#,
        r#"recorded=$(awk 'NF >= 2 { print $NF; exit }' "$checksum")"#,
        r#"actual=$(sha256sum "$archive" | awk '{print $1}')"#,
        r#"[[ "$actual" =~ ^[0-9a-fA-F]{64}$ ]]"#,
        r#"printf '%s=%s\n' "$key" "$actual" >> "$GITHUB_OUTPUT""#,
    ] {
        assert!(
            homebrew.contains(required),
            "Homebrew must use verified build artifacts: {required}"
        );
    }
    assert!(!homebrew.contains("Download sha256 files"));
    assert!(!homebrew.contains("${BASE}/${name}.tar.gz.sha256"));
    assert_before(
        homebrew,
        r#"[[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]]"#,
        "recorded=$(awk",
    );
    assert_before(homebrew, "recorded=$(awk", "actual=$(sha256sum");
    assert_before(
        homebrew,
        "actual=$(sha256sum",
        r#"[[ "$actual" =~ ^[0-9a-fA-F]{64}$ ]]"#,
    );
    assert_before(
        homebrew,
        r#"[[ "$actual" =~ ^[0-9a-fA-F]{64}$ ]]"#,
        "printf '%s=%s\\n'",
    );
}

#[test]
fn prepublish_metadata_builder_checks_fixed_archive_hashes() {
    let script = repo_file("scripts/prepare-release-metadata.py");

    for required in [
        "ARCHIVES = (",
        "HEX_SHA256 = re.compile",
        "browser_download_url",
        "checksum file names the wrong archive",
        "actual != expected",
        "json.dumps(metadata",
    ] {
        assert!(
            script.contains(required),
            "prepublish metadata helper must enforce `{required}`"
        );
    }
}

#[test]
fn dev_release_uses_the_short_calendar_prerelease_version() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("version=${BASE}-dev"));
    assert!(!workflow.contains("TIMESTAMP"));
    assert!(!workflow.contains("-dev.${TIMESTAMP}"));
}

#[test]
fn installer_instructions_use_channel_matched_release_assets() {
    let stable_unix = "https://github.com/xjoker/codex-switch/releases/latest/download/install.sh";
    let stable_windows =
        "https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1";
    let dev_unix = "https://github.com/xjoker/codex-switch/releases/download/dev/install.sh";
    let dev_windows = "https://github.com/xjoker/codex-switch/releases/download/dev/install.ps1";

    for path in [
        "README.md",
        "README_CN.md",
        "scripts/install.sh",
        "scripts/install.ps1",
        ".github/workflows/release.yml",
    ] {
        let text = repo_file(path);
        assert!(
            !text.contains("raw.githubusercontent.com/xjoker/codex-switch/master/scripts/install"),
            "{path} must not direct users to the stale installer on the master branch"
        );
    }

    for path in ["README.md", "README_CN.md"] {
        let readme = repo_file(path);
        for required in [stable_unix, stable_windows, dev_unix, dev_windows] {
            assert!(
                readme.contains(required),
                "{path} must contain channel-matched installer URL `{required}`"
            );
        }
    }

    let unix_installer = repo_file("scripts/install.sh");
    assert!(unix_installer.contains(stable_unix));
    assert!(unix_installer.contains(dev_unix));

    let windows_installer = repo_file("scripts/install.ps1");
    assert!(windows_installer.contains(stable_windows));
    assert!(windows_installer.contains(dev_windows));

    let workflow = repo_file(".github/workflows/release.yml");
    assert!(workflow.contains(stable_unix));
    assert!(workflow.contains(stable_windows));
    assert!(workflow.contains(dev_unix));
    assert!(workflow.contains(dev_windows));
}

#[test]
fn self_update_help_limits_automatic_checks_to_tui_startup() {
    let cli = repo_file("src/cli.rs");

    assert!(cli.contains("Only the TUI checks automatically at startup"));
    assert!(cli.contains("Other commands never check automatically"));
}

#[test]
fn plain_self_update_keeps_dev_installs_on_the_dev_channel() {
    let command = repo_file("src/commands/update.rs");

    assert!(command.contains("update::is_dev_version(update::current_version())"));
    assert!(command.contains("update::check_for_dev_update().await?"));
    assert!(command.contains("update::self_update_dev(show_progress).await"));
    assert!(command.contains("else if stable || version.is_some()"));
    assert_before(&command, "if dev", "else if stable || version.is_some()");
}

#[test]
fn release_docs_describe_platform_specific_archive_formats() {
    let release = repo_file("docs/RELEASE.md");

    assert!(
        release.contains("Linux / macOS") && release.contains("`.tar.gz`"),
        "release docs must describe Unix tar.gz artifacts"
    );
    assert!(
        release.contains("Windows") && release.contains("`.zip`"),
        "release docs must describe Windows zip artifacts"
    );
    assert!(
        !release.contains("6 平台 tarball"),
        "release docs must not call Windows zip artifacts tarballs"
    );
}

#[test]
fn changelog_tracks_the_calendar_version_development_cycle() {
    let changelog = repo_file("docs/CHANGELOG.md");
    assert!(
        changelog.contains("## v20260713.2.0 — 2026-07-13"),
        "the final dev candidate must carry the stable release heading before zero-drift acceptance"
    );
}
