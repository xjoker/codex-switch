//! Apply an import plan: one auth transaction, a backup of everything that is
//! replaced, a journal, and a reverse-order rollback on any error.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::LogicalKey;
use super::plan::{ImportPlan, PlanItem};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BackupEntry {
    /// Absolute path that was replaced.
    pub path: String,
    /// Its previous content, relative to the journal's directory.
    pub backup: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub state: String,
    /// Absolute paths replaced; their previous content is under `backup/`.
    pub replaced: Vec<String>,
    /// `replaced` with the backup file of each, for manual recovery.
    #[serde(default)]
    pub backups: Vec<BackupEntry>,
    /// Absolute paths created by this import.
    pub created: Vec<String>,
    /// Directories created by this import (removed on rollback when empty).
    pub created_dirs: Vec<String>,
}

pub(crate) fn backups_root() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("import-backups"))
}

/// Journals of imports that never finished (killed or powered off between
/// the first and the last write), with their directories.
fn unfinished_imports() -> Result<Vec<(PathBuf, Journal)>> {
    let root = backups_root()?;
    let Ok(read) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let journal_path = entry.path().join("journal.json");
        let Ok(raw) = std::fs::read(&journal_path) else {
            continue;
        };
        let Ok(journal) = serde_json::from_slice::<Journal>(&raw) else {
            continue;
        };
        if journal.state == "in-progress" {
            out.push((entry.path(), journal));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn recovery_steps(app: &Path, dir: &Path, journal: &Journal) -> String {
    let pairs: Vec<(String, PathBuf)> = if journal.backups.is_empty() {
        journal
            .replaced
            .iter()
            .map(|p| {
                (
                    p.clone(),
                    dir.join("backup").join(backup_relative(app, Path::new(p))),
                )
            })
            .collect()
    } else {
        journal
            .backups
            .iter()
            .map(|b| (b.path.clone(), dir.join(&b.backup)))
            .collect()
    };
    let mut msg = format!(
        "an earlier import did not finish (journal {}); some files may already hold \
         imported data while others do not. Restore it manually before importing again:\n  \
         1. copy each backup back over its original path:",
        dir.join("journal.json").display()
    );
    if pairs.is_empty() {
        msg.push_str("\n       (none: this import replaced no existing file)");
    }
    for (path, backup) in &pairs {
        msg.push_str(&format!("\n       {} -> {}", backup.display(), path));
    }
    msg.push_str("\n  2. delete each file this import created:");
    if journal.created.is_empty() {
        msg.push_str("\n       (none)");
    }
    for path in &journal.created {
        msg.push_str(&format!("\n       {path}"));
    }
    msg.push_str("\n  3. set \"state\" in the journal to \"rolled-back\"");
    msg
}

/// Refuse to start while an earlier import never finished: its journal and
/// backup are the only record of what to restore.
pub(crate) fn ensure_no_unfinished_import() -> Result<()> {
    let app = crate::auth::app_home()?;
    if let Some((dir, journal)) = unfinished_imports()?.into_iter().next() {
        anyhow::bail!("{}", recovery_steps(&app, &dir, &journal));
    }
    Ok(())
}

/// A one-line warning for commands that do not block on an unfinished import
/// (`import --dry-run`, `use`, `launch`, `export`), or `None`.
pub(crate) fn unfinished_import_warning() -> Option<String> {
    let found = unfinished_imports().ok()?;
    let (dir, _) = found.first()?;
    Some(format!(
        "an earlier import did not finish (journal {}); profiles and the live auth.json may be inconsistent. Run `codex-switch import <bundle>` to see the recovery steps.",
        dir.join("journal.json").display()
    ))
}

fn ensure_private_dir_tracked(dir: &Path, created: &mut Vec<String>) -> Result<()> {
    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(d) = cursor {
        if d.exists() {
            break;
        }
        missing.push(d.to_path_buf());
        cursor = d.parent();
    }
    for d in missing.into_iter().rev() {
        std::fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
        // Make the new directory entry itself durable, so that after a power
        // loss a file renamed into it (or the journal) is not orphaned.
        sync_parent(&d);
        created.push(d.display().to_string());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(windows)]
        crate::auth::harden_windows_private_directory(&d)?;
    }
    Ok(())
}

/// After a rename or a `create_dir`, flush the parent directory too so that,
/// after a power loss, the journal and the files it describes are on disk in
/// write order. A no-op on Windows, which has no directory fsync.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::auth::atomic_write_private(path, bytes)?;
    sync_parent(path);
    Ok(())
}

fn write_journal(dir: &Path, journal: &Journal) -> Result<()> {
    write_durable(
        &dir.join("journal.json"),
        &serde_json::to_vec_pretty(journal)?,
    )
}

fn backup_relative(app: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(app) {
        Ok(rel) => PathBuf::from("app").join(rel),
        Err(_) => PathBuf::from("live").join(path.file_name().unwrap_or_default()),
    }
}

#[cfg(debug_assertions)]
fn injected_failure(write_index: usize) -> Result<()> {
    // Test hooks (debug builds only): fail, or kill the process without any
    // cleanup, before the Nth write.
    if let Ok(n) = std::env::var("CODEX_SWITCH_TEST_IMPORT_FAIL_AT")
        && n.parse::<usize>().ok() == Some(write_index)
    {
        anyhow::bail!("injected import failure at write {write_index}");
    }
    if let Ok(n) = std::env::var("CODEX_SWITCH_TEST_IMPORT_ABORT_AT")
        && n.parse::<usize>().ok() == Some(write_index)
    {
        std::process::abort();
    }
    Ok(())
}
#[cfg(not(debug_assertions))]
fn injected_failure(_write_index: usize) -> Result<()> {
    Ok(())
}

/// Test hook (debug builds only): after the import was classified under the
/// lock and before `apply` runs, replace the live auth.json with the file
/// named by `CODEX_SWITCH_TEST_IMPORT_LIVE_BEFORE_APPLY`, standing in for
/// Codex refreshing it in that window.
#[cfg(debug_assertions)]
pub(crate) fn test_hook_before_apply() -> Result<()> {
    if let Ok(src) = std::env::var("CODEX_SWITCH_TEST_IMPORT_LIVE_BEFORE_APPLY") {
        let raw = std::fs::read(&src).with_context(|| format!("test hook: reading {src}"))?;
        std::fs::write(crate::auth::codex_auth_path()?, raw)?;
    }
    Ok(())
}
#[cfg(not(debug_assertions))]
pub(crate) fn test_hook_before_apply() -> Result<()> {
    Ok(())
}

pub(crate) struct ApplyResult {
    pub backup_dir: Option<PathBuf>,
}

struct Writes<'a> {
    item: &'a PlanItem,
    key: &'a LogicalKey,
    text: &'a str,
}

/// Execute every writing item. The caller holds the auth transaction.
pub(crate) fn apply(plan: &ImportPlan) -> Result<ApplyResult> {
    let writes: Vec<Writes> = plan
        .items
        .iter()
        .filter(|i| i.action.writes())
        .filter_map(|item| {
            item.write.as_ref().map(|(key, text)| Writes {
                item,
                key,
                text: text.as_str(),
            })
        })
        .collect();
    if writes.is_empty() {
        return Ok(ApplyResult { backup_dir: None });
    }

    let app = crate::auth::app_home()?;

    // Targets: each write, plus the live auth.json when the plan says the
    // current profile's live file is replaced too. Credentials are re-checked
    // against what is on disk now (account identity, and the newer-token rule
    // unless the user explicitly waived it) before anything is written, the
    // backup directory included.
    let live = crate::auth::codex_auth_path()?;
    let mut targets: Vec<(PathBuf, &str)> = Vec::new();
    for w in &writes {
        let path = w.key.path_in(&app);
        if let LogicalKey::ProfileAuth(alias) = w.key {
            let incoming: serde_json::Value =
                serde_json::from_str(w.text.trim_start_matches('\u{feff}'))
                    .context("parsing imported auth")?;
            crate::profile::guard_imported_credentials(
                alias,
                &path,
                &incoming,
                w.item.allow_older,
            )?;
            if w.item.replaces_live {
                crate::profile::guard_imported_live(&live, &incoming, w.item.allow_older_live)?;
            }
        }
        targets.push((path, w.text));
        if w.item.replaces_live {
            targets.push((live.clone(), w.text));
        }
    }

    let root = backups_root()?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string();
    let backup_dir = root.join(stamp);
    let mut scratch = Vec::new();
    ensure_private_dir_tracked(&backup_dir, &mut scratch)?;

    let mut journal = Journal {
        state: "in-progress".into(),
        replaced: Vec::new(),
        backups: Vec::new(),
        created: Vec::new(),
        created_dirs: Vec::new(),
    };
    for (path, _) in &targets {
        if path.exists() {
            let rel = backup_relative(&app, path);
            let dst = backup_dir.join("backup").join(&rel);
            let raw =
                std::fs::read(path).with_context(|| format!("backing up {}", path.display()))?;
            if let Some(parent) = dst.parent() {
                ensure_private_dir_tracked(parent, &mut scratch)?;
            }
            write_durable(&dst, &raw)?;
            journal.replaced.push(path.display().to_string());
            journal.backups.push(BackupEntry {
                path: path.display().to_string(),
                backup: PathBuf::from("backup")
                    .join(rel)
                    .to_string_lossy()
                    .replace('\\', "/"),
            });
        }
    }
    write_journal(&backup_dir, &journal)?;

    let result = (|| -> Result<()> {
        for (idx, (path, text)) in targets.iter().enumerate() {
            injected_failure(idx + 1)?;
            let existed = path.exists();
            if let Some(parent) = path.parent() {
                ensure_private_dir_tracked(parent, &mut journal.created_dirs)?;
            }
            if !existed {
                journal.created.push(path.display().to_string());
                write_journal(&backup_dir, &journal)?;
            }
            write_durable(path, text.as_bytes())?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            journal.state = "committed".into();
            write_journal(&backup_dir, &journal)?;
            Ok(ApplyResult {
                backup_dir: Some(backup_dir),
            })
        }
        Err(err) => {
            let rollback = rollback(&app, &backup_dir, &mut journal);
            match rollback {
                Ok(()) => Err(err.context(format!(
                    "import failed and was rolled back; nothing was changed (journal {})",
                    backup_dir.join("journal.json").display()
                ))),
                Err(rb) => Err(err.context(format!(
                    "import failed AND rollback failed ({rb:#}); restore manually from {}",
                    backup_dir.display()
                ))),
            }
        }
    }
}

fn rollback(app: &Path, backup_dir: &Path, journal: &mut Journal) -> Result<()> {
    for path in journal.created.iter().rev() {
        let p = Path::new(path);
        if p.exists() {
            std::fs::remove_file(p).with_context(|| format!("removing {path}"))?;
        }
    }
    for path in &journal.replaced {
        let p = Path::new(path);
        let src = backup_dir.join("backup").join(backup_relative(app, p));
        let raw = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        crate::auth::atomic_write_private(p, &raw)?;
    }
    for dir in journal.created_dirs.iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }
    journal.state = "rolled-back".into();
    write_journal(backup_dir, journal)
}
