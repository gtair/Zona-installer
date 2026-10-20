//! Hand-rolled logger: one timestamped file per run, aligned columns, console
//! at DEBUG when enabled.
//!
//! [`init_logger`] creates a per-run `logs\log_YYYY-MM-DD_HH-MM-SS.log` file and
//! installs a global [`log::Log`] sink. Each record is rendered as
//!
//! ```text
//! HH:MM:SS.mmm [LEVEL   ] source - message
//! ```
//!
//! and mirrored to the file and console under independent filters: the file is
//! INFO (or DEBUG when `log_debug`); the console is DEBUG only when
//! `debug_console`. Dropping the [`LogHandle`] flushes and closes the file.
//!
//! Timestamps are UTC civil time; the formatting is split from the single clock
//! read ([`now_civil`]) so it is unit-testable.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::error::{AppError, Result};

/// Width of the level column inside the brackets (the widest level name,
/// `CRITICAL`, is 8).
const LEVEL_WIDTH: usize = 8;

/// Width of the fixed source column (`source:func`); a longer source is written
/// in full, so this is a minimum width.
const SOURCE_WIDTH: usize = 20;

/// Upper bound on same-second collision suffixes before giving up with an error.
const MAX_COLLISIONS: u32 = 1000;

/// A snapshot of a wall-clock instant in civil (UTC) form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CivilTime {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millis: u16,
}

/// The global sink. `CURRENT` always points at the most recently initialised
/// handle's sink, so a second [`init_logger`] redirects new records without
/// re-installing the global logger.
static CURRENT: Mutex<Option<Arc<Sink>>> = Mutex::new(None);

/// Installs the global [`log`] sink exactly once.
static INSTALL: Once = Once::new();

/// Serializes tests that touch the process-global logger state.
///
/// `cargo test` runs tests in parallel, and the logger state is shared by the
/// whole process, so any test that installs a logger must hold this lock. It is
/// `pub(crate)` so the `app` wiring test can share the SAME lock.
#[cfg(test)]
pub(crate) static LOG_LOCK: Mutex<()> = Mutex::new(());

/// The single global logger; it forwards to whatever [`CURRENT`] points at.
static GLOBAL_LOGGER: GlobalLogger = GlobalLogger;

/// Owns one run's log file and the writer state used by the global logger.
struct Sink {
    /// The log file path this sink writes to (also exposed by
    /// [`current_log_path`]).
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    file_level: LevelFilter,
    console_level: LevelFilter,
}

impl Sink {
    fn emit(&self, record: &Record<'_>) {
        let level = record.level().to_level_filter();
        let source = if record.target().is_empty() {
            record.module_path().unwrap_or("?")
        } else {
            record.target()
        };
        let line = format_line(
            &now_civil(),
            record.level(),
            source,
            &record.args().to_string(),
        );

        if level <= self.file_level {
            if let Ok(mut guard) = self.file.lock() {
                if let Some(file) = guard.as_mut() {
                    let _ = writeln!(file, "{line}");
                }
            }
        }

        if level <= self.console_level {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{line}");
        }
    }
}

/// The global [`Log`] implementation, forwarding records to [`CURRENT`].
struct GlobalLogger;

impl Log for GlobalLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let level = metadata.level().to_level_filter();
        match CURRENT.lock() {
            Ok(guard) => guard
                .as_ref()
                .is_some_and(|sink| level <= sink.file_level || level <= sink.console_level),
            Err(_) => false,
        }
    }

    fn log(&self, record: &Record<'_>) {
        let Ok(guard) = CURRENT.lock() else {
            return;
        };
        if let Some(sink) = guard.as_ref() {
            sink.emit(record);
        }
    }

    fn flush(&self) {
        if let Ok(guard) = CURRENT.lock() {
            if let Some(sink) = guard.as_ref() {
                if let Ok(mut file) = sink.file.lock() {
                    if let Some(file) = file.as_mut() {
                        let _ = file.flush();
                    }
                }
            }
        }
        let _ = std::io::stderr().flush();
    }
}

/// Handle for the current run's log file.
///
/// Dropping the handle flushes and closes the file and, if this handle was the
/// active sink, stops the global logger from writing to it.
pub struct LogHandle {
    /// The log file created for this run.
    pub path: PathBuf,
    sink: Arc<Sink>,
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        if let Ok(mut current) = CURRENT.lock() {
            if current
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.sink))
            {
                *current = None;
            }
        }
        if let Ok(mut file) = self.sink.file.lock() {
            if let Some(mut file) = file.take() {
                let _ = file.flush();
            }
        }
    }
}

/// The path of the currently active run log, if one has been initialised.
///
/// Returns `None` before [`init_logger`] runs or after the active [`LogHandle`]
/// is dropped.
pub fn current_log_path() -> Option<PathBuf> {
    CURRENT.lock().ok()?.as_ref().map(|sink| sink.path.clone())
}

/// The independent `(file, console)` level filters for a run.
fn levels(log_debug: bool, debug_console: bool) -> (LevelFilter, LevelFilter) {
    let file = if log_debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    let console = if debug_console {
        LevelFilter::Debug
    } else {
        LevelFilter::Off
    };
    (file, console)
}

/// Creates this run's timestamped log file and installs the global logger.
///
/// The file is `log_YYYY-MM-DD_HH-MM-SS.log` under `log_dir`; on a name clash a
/// `_N` suffix is appended. The two flags are honoured independently
/// ([`levels`]).
pub fn init_logger(log_dir: &Path, log_debug: bool, debug_console: bool) -> Result<LogHandle> {
    std::fs::create_dir_all(log_dir).map_err(|source| AppError::Io {
        context: "create log directory".to_string(),
        path: Some(log_dir.to_path_buf()),
        source,
    })?;

    let now = now_civil();
    let path = unique_log_path(log_dir, &now)?;

    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|source| AppError::Io {
            context: "create log file".to_string(),
            path: Some(path.clone()),
            source,
        })?;

    let (file_level, console_level) = levels(log_debug, debug_console);

    let sink = Arc::new(Sink {
        path: path.clone(),
        file: Mutex::new(Some(file)),
        file_level,
        console_level,
    });

    INSTALL.call_once(|| {
        // `log` accepts at most one global logger; this install is best-effort.
        let _ = log::set_logger(&GLOBAL_LOGGER);
    });
    // The global gate must admit the max of both levels, or one sink starves the
    // other.
    log::set_max_level(std::cmp::max(file_level, console_level));

    if let Ok(mut current) = CURRENT.lock() {
        *current = Some(Arc::clone(&sink));
    }

    Ok(LogHandle { path, sink })
}

/// Chooses a not-yet-existing log file path for `t`, suffixing `_N` on clash.
fn unique_log_path(log_dir: &Path, t: &CivilTime) -> Result<PathBuf> {
    let base = log_file_name(t);
    let first = log_dir.join(&base);
    if !first.exists() {
        return Ok(first);
    }

    let stem = base.strip_suffix(".log").unwrap_or(base.as_str());
    for n in 1..=MAX_COLLISIONS {
        let candidate = log_dir.join(format!("{stem}_{n}.log"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(AppError::Io {
        context: "allocate unique log file name".to_string(),
        path: Some(log_dir.to_path_buf()),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free log file name after 1000 collisions",
        ),
    })
}

/// The canonical `log_YYYY-MM-DD_HH-MM-SS.log` file name for `t`.
fn log_file_name(t: &CivilTime) -> String {
    format!(
        "log_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.log",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// Renders one aligned log line (without the trailing newline).
fn format_line(t: &CivilTime, level: Level, source: &str, message: &str) -> String {
    format!(
        "{clock} [{level:<level_width$}] {source:<source_width$} - {message}",
        clock = format_clock(t),
        level = level.as_str(),
        level_width = LEVEL_WIDTH,
        source = source,
        source_width = SOURCE_WIDTH,
        message = message,
    )
}

/// Renders the `HH:MM:SS.mmm` clock column for `t`.
fn format_clock(t: &CivilTime) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        t.hour, t.minute, t.second, t.millis
    )
}

/// Reads the current instant and converts it to UTC civil time.
///
/// This is the only impure clock read.
fn now_civil() -> CivilTime {
    let (secs, millis) = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_millis()),
        Err(error) => {
            // The clock is before the Unix epoch (never in practice): floor to
            // whole seconds with a non-negative sub-second remainder.
            let duration = error.duration();
            let mut secs = -(duration.as_secs() as i64);
            let mut millis = duration.subsec_millis();
            if millis != 0 {
                secs -= 1;
                millis = 1000 - millis;
            }
            (secs, millis)
        }
    };
    from_unix(secs, millis)
}

/// Converts a Unix instant (seconds + milliseconds since epoch) to civil time.
fn from_unix(secs: i64, millis: u32) -> CivilTime {
    let days = secs.div_euclid(86_400);
    let second_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    CivilTime {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: (second_of_day / 3600) as u8,
        minute: ((second_of_day % 3600) / 60) as u8,
        second: (second_of_day % 60) as u8,
        millis: millis as u16,
    }
}

/// Converts a day count since 1970-01-01 to `(year, month, day)`.
///
/// Howard Hinnant's civil-from-days algorithm; all arithmetic is on `i64`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SAMPLE: CivilTime = CivilTime {
        year: 2026,
        month: 10,
        day: 9,
        hour: 13,
        minute: 5,
        second: 7,
        millis: 42,
    };

    /// Creates a unique temp directory for a logging test.
    fn temp_log_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "zona_installer_logging_{label}_{}_{nanos}_{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Acquires the shared logger lock, recovering from a poisoned mutex.
    fn log_lock() -> std::sync::MutexGuard<'static, ()> {
        LOG_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Counts the `log_*.log` files directly under `dir`.
    fn log_file_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        name.starts_with("log_") && name.ends_with(".log")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn format_line_matches_the_aligned_pattern() {
        let line = format_line(&SAMPLE, Level::Info, "installer:main", "starting up");
        assert_eq!(
            line,
            "13:05:07.042 [INFO    ] installer:main       - starting up"
        );
    }

    #[test]
    fn message_column_is_aligned_across_levels() {
        let error = format_line(&SAMPLE, Level::Error, "a:b", "x");
        let trace = format_line(&SAMPLE, Level::Trace, "a:b", "x");
        let error_at = error.find(" - x").expect("separator present");
        let trace_at = trace.find(" - x").expect("separator present");
        assert_eq!(error_at, trace_at);
    }

    #[test]
    fn log_file_name_uses_the_run_timestamp() {
        assert_eq!(log_file_name(&SAMPLE), "log_2026-10-09_13-05-07.log");
    }

    #[test]
    fn from_unix_converts_known_instants() {
        assert_eq!(
            from_unix(0, 0),
            CivilTime {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
                millis: 0,
            }
        );
        assert_eq!(
            from_unix(1_700_000_000, 123),
            CivilTime {
                year: 2023,
                month: 11,
                day: 14,
                hour: 22,
                minute: 13,
                second: 20,
                millis: 123,
            }
        );
        // 2000-02-29T00:00:00Z, exercising the leap-day path.
        assert_eq!(
            from_unix(951_782_400, 0),
            CivilTime {
                year: 2000,
                month: 2,
                day: 29,
                hour: 0,
                minute: 0,
                second: 0,
                millis: 0,
            }
        );
    }

    #[test]
    fn levels_map_the_two_flags_independently() {
        // (file, console) for every combination of (log_debug, debug_console).
        assert_eq!(levels(false, false), (LevelFilter::Info, LevelFilter::Off));
        assert_eq!(levels(true, false), (LevelFilter::Debug, LevelFilter::Off));
        // The console is DEBUG even though the file stays INFO.
        assert_eq!(levels(false, true), (LevelFilter::Info, LevelFilter::Debug));
        assert_eq!(levels(true, true), (LevelFilter::Debug, LevelFilter::Debug));
    }

    #[test]
    fn two_inits_create_distinct_timestamped_files_and_write_an_aligned_line() {
        let _guard = log_lock();
        let dir = temp_log_dir("two_inits");

        let first = init_logger(&dir, true, false).unwrap();
        let second = init_logger(&dir, true, false).unwrap();

        assert_ne!(
            first.path, second.path,
            "two inits must create distinct files"
        );
        assert!(first.path.exists());
        assert!(second.path.exists());

        let name_pattern =
            Regex::new(r"^log_\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}(_\d+)?\.log$").unwrap();
        for handle in [&first, &second] {
            let name = handle
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("log file has a name");
            assert!(
                name_pattern.is_match(name),
                "log file name is not timestamped: {name:?}"
            );
        }

        // The second init is the active sink, so the record lands in its file.
        log::info!(target: "src:func", "hello world");
        let written = second.path.clone();
        drop(first);
        drop(second);

        let contents = std::fs::read_to_string(&written).unwrap();
        let line = contents
            .lines()
            .find(|line| line.contains("hello world"))
            .expect("the logged line was written");
        let line_pattern =
            Regex::new(r"^\d{2}:\d{2}:\d{2}\.\d{3} \[INFO\s*\] src:func\s+ - hello world$")
                .unwrap();
        assert!(
            line_pattern.is_match(line),
            "line does not match the aligned pattern: {line:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_log_path_avoids_clobbering_an_existing_file() {
        let dir = temp_log_dir("collision");
        let canonical = dir.join(log_file_name(&SAMPLE));
        std::fs::write(&canonical, b"previous run").unwrap();

        let chosen = unique_log_path(&dir, &SAMPLE).unwrap();
        assert_ne!(chosen, canonical);
        assert_eq!(
            chosen.file_name().and_then(|n| n.to_str()),
            Some("log_2026-10-09_13-05-07_1.log")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_log_records_info_but_not_debug_when_log_debug_is_false() {
        // Guarded: this installs the process-global sink.
        let _guard = log_lock();
        let dir = temp_log_dir("info_only");

        // The console flag is ON, but the file level must still be INFO.
        let handle = init_logger(&dir, false, true).unwrap();
        log::info!(target: "src:func", "INFO marker line");
        log::debug!(target: "src:func", "DEBUG marker line");
        let path = handle.path.clone();
        drop(handle);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("INFO marker line"),
            "the INFO line must be recorded: {contents:?}"
        );
        assert!(
            !contents.contains("DEBUG marker line"),
            "DEBUG must not reach a log_debug=false file even with the console on: {contents:?}"
        );
        assert_eq!(log_file_count(&dir), 1, "exactly one log file per run");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_log_records_debug_when_log_debug_is_true() {
        let _guard = log_lock();
        let dir = temp_log_dir("debug_on");

        let handle = init_logger(&dir, true, false).unwrap();
        log::info!(target: "src:func", "INFO marker line");
        log::debug!(target: "src:func", "DEBUG marker line");
        let path = handle.path.clone();
        drop(handle);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("INFO marker line"),
            "the INFO line must be recorded: {contents:?}"
        );
        assert!(
            contents.contains("DEBUG marker line"),
            "the DEBUG line must be recorded when log_debug=true: {contents:?}"
        );
        assert_eq!(log_file_count(&dir), 1, "exactly one log file per run");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
