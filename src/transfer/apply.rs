//! Apply an import plan: one auth transaction, a backup of everything that is
//! replaced, a journal, and a reverse-order rollback on any error.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::LogicalKey;
use super::plan::{ImportPlan, PlanItem};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Journal {
    pub state: String,
    /// Absolute paths replaced; their previous content is under `backup/`.
    pub replaced: Vec<String>,
    /// Absolute paths created by this import.
    pub created: Vec<String>,
    /// Directories created by this import (removed on rollback when empty).
    pub created_dirs: Vec<String>,
}

pub(crate) fn backups_root() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("import-backups"))
}

/// Refuse to start while an earlier import never finished: its journal and
/// backup are the only record of what to restore.
pub(crate) fn ensure_no_unfinished_import() -> Result<()> {
    let root = backups_root()?;
    let Ok(read) = std::fs::read_dir(&root) else {
        return Ok(());
    };
    for entry in read.flatten() {
        let journal_path = entry.path().join("journal.json");
        let Ok(raw) = std::fs::read(&journal_path) else {
            continue;
        };
        let Ok(journal) = serde_json::from_slice::<Journal>(&raw) else {
            continue;
        };
        if journal.state == "in-progress" {
            anyhow::bail!(
                "an earlier import did not finish (journal {}).\nRestore it manually before \
                 importing again:\n  1. copy each file under {} back to the path listed in \
                 `replaced`\n  2. delete each path listed in `created`\n  3. set \"state\" in the \
                 journal to \"rolled-back\"",
                journal_path.display(),
                entry.path().join("backup").display()
            );
        }
    }
    Ok(())
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

fn write_journal(dir: &Path, journal: &Journal) -> Result<()> {
    crate::auth::atomic_write_private(
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
    // Test hook (debug builds only): fail before the Nth write.
    if let Ok(n) = std::env::var("CODEX_SWITCH_TEST_IMPORT_FAIL_AT")
        && n.parse::<usize>().ok() == Some(write_index)
    {
        anyhow::bail!("injected import failure at write {write_index}");
    }
    Ok(())
}
#[cfg(not(debug_assertions))]
fn injected_failure(_write_index: usize) -> Result<()> {
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
    let root = backups_root()?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ").to_string();
    let backup_dir = root.join(stamp);
    let mut scratch = Vec::new();
    ensure_private_dir_tracked(&backup_dir, &mut scratch)?;

    // Targets: each write, plus the live auth.json when a current profile changes.
    let live = crate::auth::codex_auth_path()?;
    let mut targets: Vec<(PathBuf, &str)> = Vec::new();
    for w in &writes {
        targets.push((w.key.path_in(&app), w.text));
        if w.item.updates_live {
            targets.push((live.clone(), w.text));
        }
    }

    let mut journal = Journal {
        state: "in-progress".into(),
        replaced: Vec::new(),
        created: Vec::new(),
        created_dirs: Vec::new(),
    };
    for (path, _) in &targets {
        if path.exists() {
            let rel = backup_relative(&app, path);
            let dst = backup_dir.join("backup").join(rel);
            let raw =
                std::fs::read(path).with_context(|| format!("backing up {}", path.display()))?;
            if let Some(parent) = dst.parent() {
                ensure_private_dir_tracked(parent, &mut scratch)?;
            }
            crate::auth::atomic_write_private(&dst, &raw)?;
            journal.replaced.push(path.display().to_string());
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
            crate::auth::atomic_write_private(path, text.as_bytes())?;
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
