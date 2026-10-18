//! The log file, for finding out later why something failed. It holds what
//! happened, never message text, names, passwords, codes, tokens or keys
//! (see what the core logs).
//!
//! `corvo-chat.log` sits in the data folder, readable only by you. Past 1 MB
//! it becomes `corvo-chat.log.old` (replacing the last one) and a new file
//! starts. Warnings only, unless `CORVO_CHAT_LOG` names another level
//! (`info`, `debug`, `trace`). Only this app's own code is ever logged: the
//! libraries under it may log addresses and more.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

use crate::files::LOG_FILE;

/// Where the log goes when it's full.
const OLD_LOG_FILE: &str = "corvo-chat.log.old";
const MAX_BYTES: u64 = 1024 * 1024;
/// Set to change what's logged.
const LEVEL_VAR: &str = "CORVO_CHAT_LOG";

/// The app's log file, kept for as long as the app runs.
static LOG: OnceLock<LogFile> = OnceLock::new();

/// Sends the app's and the core's log lines to the log file in `dir`.
/// Does nothing if logging was already set up.
pub fn start(dir: &Path) {
    let setting = std::env::var(LEVEL_VAR).ok();
    if LOG.set(LogFile::new(dir)).is_ok() {
        let log = LOG.get().expect("set just now");
        let _ = subscriber(log, filter(setting.as_deref())).try_init();
    }
}

/// This app's own code at the level `setting` names (warnings if it names
/// none), and nothing from the libraries under it.
fn filter(setting: Option<&str>) -> Targets {
    let level = setting.and_then(|s| s.trim().parse::<LevelFilter>().ok()).unwrap_or(LevelFilter::WARN);
    Targets::new().with_target("corvo_chat_core", level).with_target("corvo_chat", level)
}

fn subscriber(file: &'static LogFile, filter: Targets) -> impl tracing::Subscriber + Send + Sync {
    let lines = tracing_subscriber::fmt::layer().with_writer(file).with_ansi(false);
    tracing_subscriber::registry().with(lines.with_filter(filter))
}

/// The log file, opened on the first line written.
pub struct LogFile {
    path: PathBuf,
    old: PathBuf,
    limit: u64,
    file: Mutex<Option<File>>,
}

impl LogFile {
    pub fn new(dir: &Path) -> LogFile {
        LogFile::with_limit(dir, MAX_BYTES)
    }

    fn with_limit(dir: &Path, limit: u64) -> LogFile {
        LogFile { path: dir.join(LOG_FILE), old: dir.join(OLD_LOG_FILE), limit, file: Mutex::new(None) }
    }

    fn append(&self, line: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        if file.is_none() {
            *file = Some(open(&self.path)?);
        }
        let size = file.as_ref().map_or(Ok(0), |f| f.metadata().map(|m| m.len()))?;
        if size > 0 && size + line.len() as u64 > self.limit {
            *file = None;
            fs::rename(&self.path, &self.old)?;
            *file = Some(open(&self.path)?);
        }
        file.as_mut().expect("opened above").write_all(line)
    }
}

/// Appends, readable only by you on Unix (the folder is private on Windows).
fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

/// One log line on its way to the file. A line that can't be written (the
/// data folder isn't there, the disk is full) is dropped: logging must never
/// stop the app.
pub struct LineWriter<'a>(&'a LogFile);

impl Write for LineWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.0.append(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for &'static LogFile {
    type Writer = LineWriter<'a>;

    fn make_writer(&'a self) -> LineWriter<'a> {
        LineWriter(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_is_private_and_one_old_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let log = LogFile::with_limit(dir.path(), 100);
        for i in 0..5 {
            log.append(format!("line {i} {}\n", "x".repeat(40)).as_bytes()).unwrap();
        }
        let (now, old) = (fs::read_to_string(dir.path().join(LOG_FILE)).unwrap(), dir.path().join(OLD_LOG_FILE));
        assert_eq!(now, format!("line 4 {}\n", "x".repeat(40)), "a full log starts over");
        assert!(fs::read_to_string(&old).unwrap().starts_with("line 2"), "the last full one is kept");
        assert!(!dir.path().join("corvo-chat.log.old.old").exists());
        #[cfg(unix)]
        for file in [dir.path().join(LOG_FILE), old] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn the_level_setting_never_lets_in_other_libraries_logs() {
        use tracing::Level;
        let debug = filter(Some("debug"));
        assert!(debug.would_enable("corvo_chat_core::native::driver", &Level::DEBUG));
        assert!(debug.would_enable("corvo_chat::ui", &Level::DEBUG));
        // The TLS and WebSocket libraries' debug lines hold addresses and frames.
        assert!(!debug.would_enable("rustls::client::hs", &Level::DEBUG));
        assert!(!debug.would_enable("tokio_tungstenite", &Level::ERROR));
        for usual in [filter(None), filter(Some("not a level")), filter(Some("rustls=trace"))] {
            assert!(usual.would_enable("corvo_chat_core", &Level::WARN));
            assert!(!usual.would_enable("corvo_chat_core", &Level::INFO));
            assert!(!usual.would_enable("rustls", &Level::TRACE));
        }
    }

    #[test]
    fn a_line_with_nowhere_to_go_is_dropped_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let log = LogFile::new(&dir.path().join("not made yet"));
        assert_eq!(LineWriter(&log).write(b"lost\n").unwrap(), 5);
    }
}
