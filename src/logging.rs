use anyhow::{Context, Result};
use chrono::{Days, Local, NaiveDate};
use fs4::FileExt;
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

const LOG_PREFIX: &str = "codex-switch";
const MAX_LOG_AGE_DAYS: u64 = 3;
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TUI_LOG_LINES: usize = 1000;
static TUI_LOG_WRITER: OnceLock<TuiLogWriter> = OnceLock::new();

pub(crate) fn tui_log_writer() -> TuiLogWriter {
    TUI_LOG_WRITER.get_or_init(TuiLogWriter::new).clone()
}

#[derive(Clone)]
pub(crate) struct TuiLogWriter {
    state: Arc<Mutex<TuiLogState>>,
}

struct TuiLogState {
    lines: VecDeque<String>,
    capacity: usize,
    revision: u64,
}

impl TuiLogWriter {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TuiLogState {
                lines: VecDeque::new(),
                capacity: MAX_TUI_LOG_LINES,
                revision: 0,
            })),
        }
    }

    #[cfg(test)]
    fn new_for_test(capacity: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(TuiLogState {
                lines: VecDeque::new(),
                capacity,
                revision: 0,
            })),
        }
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn lines_if_changed(
        &self,
        previous_revision: Option<u64>,
    ) -> Option<(u64, Vec<String>)> {
        let state = self.state.lock().ok()?;
        (previous_revision != Some(state.revision)).then(|| {
            (
                state.revision,
                state.lines.iter().cloned().collect::<Vec<_>>(),
            )
        })
    }
}

impl<'a> MakeWriter<'a> for TuiLogWriter {
    type Writer = TuiLogSink;

    fn make_writer(&'a self) -> Self::Writer {
        TuiLogSink {
            state: Arc::clone(&self.state),
        }
    }
}

pub(crate) struct TuiLogSink {
    state: Arc<Mutex<TuiLogState>>,
}

impl Write for TuiLogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("TUI log writer lock poisoned"))?;
        let mut changed = false;
        for line in String::from_utf8_lossy(buf).lines() {
            if state.lines.len() == state.capacity {
                state.lines.pop_front();
            }
            state.lines.push_back(line.to_string());
            changed = true;
        }
        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How long retention may go unenforced, and how many bytes may be appended in
/// the meantime.
///
/// `tracing` calls `Write::write` once per record and the retention scan walks
/// the log directory, so running it per record made every debug-level log line
/// a directory walk. Retention only has to be approximately timely: whichever
/// of these two is reached first triggers the next scan, which bounds how far
/// the directory can drift past [`MAX_LOG_BYTES`] to one byte budget.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const MAINTENANCE_BYTE_BUDGET: u64 = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct FileLogWriter {
    state: Arc<Mutex<LogState>>,
}

struct LogState {
    dir: PathBuf,
    /// When retention was last enforced; `None` until the first record.
    last_maintenance: Option<Instant>,
    /// Bytes appended since that enforcement.
    bytes_since_maintenance: u64,
}

pub(crate) fn file_log_writer() -> Result<FileLogWriter> {
    let dir = crate::auth::app_home()?.join("logs");
    create_private_log_dir(&dir)
        .with_context(|| format!("creating log directory {}", dir.display()))?;
    Ok(FileLogWriter {
        state: Arc::new(Mutex::new(LogState {
            dir,
            last_maintenance: None,
            bytes_since_maintenance: 0,
        })),
    })
}

fn create_private_log_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

#[cfg(unix)]
fn tighten_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

impl<'a> MakeWriter<'a> for FileLogWriter {
    type Writer = LogFile;

    fn make_writer(&'a self) -> Self::Writer {
        LogFile {
            state: Arc::clone(&self.state),
        }
    }
}

pub(crate) struct LogFile {
    state: Arc<Mutex<LogState>>,
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let retained = if buf.len() as u64 > MAX_LOG_BYTES {
            &buf[buf.len() - MAX_LOG_BYTES as usize..]
        } else {
            buf
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("log writer lock poisoned"))?;
        state.bytes_since_maintenance = state
            .bytes_since_maintenance
            .saturating_add(retained.len() as u64);
        let now = Instant::now();
        let run_maintenance =
            maintenance_due(state.last_maintenance, now, state.bytes_since_maintenance);
        append_log(
            &state.dir,
            Local::now().date_naive(),
            retained,
            run_maintenance,
        )?;
        if run_maintenance {
            state.last_maintenance = Some(now);
            state.bytes_since_maintenance = 0;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether this record should also pay for a retention scan.
///
/// The first record of a process always does — nothing earlier can have done
/// it — and after that whichever of the byte budget or the interval arrives
/// first.
fn maintenance_due(last: Option<Instant>, now: Instant, bytes_since: u64) -> bool {
    let Some(last) = last else {
        return true;
    };
    bytes_since >= MAINTENANCE_BYTE_BUDGET || now.duration_since(last) >= MAINTENANCE_INTERVAL
}

fn append_log(dir: &Path, today: NaiveDate, bytes: &[u8], run_maintenance: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut lock_options = OpenOptions::new();
    lock_options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let lock = lock_options.open(dir.join(".lock"))?;
    #[cfg(unix)]
    tighten_file_permissions(&lock)?;
    FileExt::lock(&lock)?;
    let result = (|| {
        if run_maintenance {
            run_log_maintenance(dir, today, bytes.len() as u64)?;
        }
        let mut log_options = OpenOptions::new();
        log_options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            log_options.mode(0o600);
        }
        let mut file = log_options.open(log_path(dir, today))?;
        #[cfg(unix)]
        tighten_file_permissions(&file)?;
        file.write_all(bytes)
    })();
    FileExt::unlock(&lock)?;
    result
}

/// Drop log files outside the retention window.
///
/// Size enforcement used to be nested at the end of this, which meant a single
/// append ran three directory scans: this one, the nested one, and the caller's.
/// The two passes are now siblings under [`run_log_maintenance`], so an append
/// that does maintenance costs two scans and one that does not costs none.
fn prune_expired_log_files(dir: &Path, today: NaiveDate) -> io::Result<()> {
    let oldest = today - Days::new(MAX_LOG_AGE_DAYS - 1);
    for (path, date, _) in log_files(dir)? {
        if date < oldest {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Age retention, then size retention accounting for the record about to be
/// written.
fn run_log_maintenance(dir: &Path, today: NaiveDate, incoming: u64) -> io::Result<()> {
    prune_expired_log_files(dir, today)?;
    enforce_log_size_limit(dir, today, incoming)
}

fn enforce_log_size_limit(dir: &Path, today: NaiveDate, incoming: u64) -> io::Result<()> {
    let current = log_path(dir, today);
    let mut files = log_files(dir)?;
    files.sort_by_key(|(_, date, _)| *date);
    let mut total = files.iter().map(|(_, _, size)| *size).sum::<u64>();

    for (path, _, size) in &files {
        if total.saturating_add(incoming) <= MAX_LOG_BYTES {
            return Ok(());
        }
        if *path != current {
            fs::remove_file(path)?;
            total = total.saturating_sub(*size);
        }
    }

    if total.saturating_add(incoming) > MAX_LOG_BYTES && current.exists() {
        fs::OpenOptions::new()
            .write(true)
            .open(&current)?
            .set_len(0)?;
    }
    Ok(())
}

fn log_files(dir: &Path) -> io::Result<Vec<(PathBuf, NaiveDate, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(date) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(log_date)
        else {
            continue;
        };
        if entry.file_type()?.is_file() {
            files.push((path, date, entry.metadata()?.len()));
        }
    }
    Ok(files)
}

fn log_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{LOG_PREFIX}.{date}.log"))
}

fn log_date(filename: &str) -> Option<NaiveDate> {
    filename
        .strip_prefix(&format!("{LOG_PREFIX}."))?
        .strip_suffix(".log")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_writer_keeps_logs_in_memory_without_terminal_output() {
        let writer = TuiLogWriter::new_for_test(3);
        let mut sink = writer.make_writer();
        sink.write_all(b"first\nsecond\n").unwrap();

        let (revision, lines) = writer.lines_if_changed(None).unwrap();
        assert_eq!(lines, vec!["first", "second"]);
        assert!(writer.lines_if_changed(Some(revision)).is_none());

        sink.write_all(b"third\n").unwrap();
        assert_eq!(
            writer.lines_if_changed(Some(revision)).unwrap().1,
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn tui_writer_discards_oldest_lines_at_capacity() {
        let writer = TuiLogWriter::new_for_test(2);
        let mut sink = writer.make_writer();
        sink.write_all(b"first\nsecond\nthird\n").unwrap();

        assert_eq!(writer.lines(), vec!["second", "third"]);
    }

    fn create_log(dir: &Path, day: NaiveDate, bytes: u64) {
        let file = fs::File::create(log_path(dir, day)).unwrap();
        file.set_len(bytes).unwrap();
    }

    #[test]
    fn retains_only_the_latest_three_calendar_days() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        for day in 8..=12 {
            create_log(
                dir.path(),
                NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
                1,
            );
        }

        prune_expired_log_files(dir.path(), today).unwrap();

        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 8).unwrap()).exists());
        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()).exists());
        assert!(log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()).exists());
        assert!(log_path(dir.path(), today).exists());
    }

    #[test]
    fn removes_oldest_logs_to_keep_total_at_ten_mebibytes() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        for day in 10..=12 {
            create_log(
                dir.path(),
                NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
                5 * 1024 * 1024,
            );
        }

        run_log_maintenance(dir.path(), today, 0).unwrap();

        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()).exists());
        assert!(log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 11).unwrap()).exists());
        assert!(log_path(dir.path(), today).exists());
    }

    #[test]
    fn appending_never_exceeds_ten_mebibytes() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        create_log(dir.path(), today, MAX_LOG_BYTES);

        append_log(dir.path(), today, b"next event", true).unwrap();

        assert!(fs::metadata(log_path(dir.path(), today)).unwrap().len() <= MAX_LOG_BYTES);
    }

    // ── retention runs on a budget, not on every record ────────
    //
    // `tracing` calls `write` once per log record, and the retention scan is
    // several `read_dir` passes over the log directory. At debug level that
    // turned every single log line into a directory walk.

    #[test]
    fn maintenance_runs_only_on_first_write_or_after_a_budget_is_spent() {
        let now = Instant::now();
        assert!(maintenance_due(None, now, 0));
        assert!(!maintenance_due(Some(now), now, 64));
        assert!(maintenance_due(Some(now), now, MAINTENANCE_BYTE_BUDGET));
        let last = now.checked_sub(MAINTENANCE_INTERVAL).unwrap();
        assert!(maintenance_due(Some(last), now, 1));
    }

    /// The wiring, not just the decision: a write that is not due must leave
    /// out-of-retention files alone, and a due one must still collect them.
    #[test]
    fn a_skipped_maintenance_write_does_not_scan_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let expired = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        create_log(dir.path(), expired, 1);

        append_log(dir.path(), today, b"skipped\n", false).unwrap();
        assert!(
            log_path(dir.path(), expired).exists(),
            "a write that is not due for maintenance must not walk the log directory"
        );

        append_log(dir.path(), today, b"due\n", true).unwrap();
        assert!(
            !log_path(dir.path(), expired).exists(),
            "a write that is due must still apply retention"
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_log_tightens_directory_lock_and_log_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let lock_path = dir.path().join(".lock");
        let current_log = log_path(dir.path(), today);
        fs::File::create(&lock_path).unwrap();
        fs::File::create(&current_log).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(&current_log, fs::Permissions::from_mode(0o666)).unwrap();

        append_log(dir.path(), today, b"private event", true).unwrap();

        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(current_log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
