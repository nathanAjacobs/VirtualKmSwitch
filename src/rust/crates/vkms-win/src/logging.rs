//! A small logger for the `log` facade: one line per record, to stderr or a file.
//!
//! The agent has no console and the service usually has none either, so a file is the normal
//! destination. Logging must never take a process down: write failures are swallowed.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// A file larger than this at startup is moved aside rather than appended to without limit.
const ROTATE_AT_BYTES: u64 = 8 * 1024 * 1024;

enum Destination {
    Stderr,
    File(Mutex<File>),
}

struct Logger {
    destination: Destination,
    level: LevelFilter,
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = format!(
            "{} [{:>6}] {:<5} {}: {}\n",
            timestamp(),
            std::process::id(),
            record.level(),
            record.target(),
            record.args()
        );

        match &self.destination {
            Destination::Stderr => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            Destination::File(file) => {
                if let Ok(mut file) = file.lock() {
                    let _ = file.write_all(line.as_bytes());
                }
            }
        }
    }

    fn flush(&self) {
        if let Destination::File(file) = &self.destination {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
    }
}

fn timestamp() -> String {
    let time = unsafe {
        let mut time = std::mem::zeroed();
        GetLocalTime(&mut time);
        time
    };

    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond, time.wMilliseconds
    )
}

/// Logs to stderr. For interactive runs.
pub fn init_stderr(level: LevelFilter) {
    install(Logger { destination: Destination::Stderr, level });
}

/// Logs to `path`, creating its directory. Falls back to stderr if the file cannot be opened.
pub fn init_file(path: &Path, level: LevelFilter) {
    match open_rotated(path) {
        Ok(file) => install(Logger { destination: Destination::File(Mutex::new(file)), level }),
        Err(_) => init_stderr(level),
    }
}

fn open_rotated(path: &Path) -> std::io::Result<File> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)?;
    }

    if fs::metadata(path).map(|metadata| metadata.len() > ROTATE_AT_BYTES).unwrap_or(false) {
        let mut previous = PathBuf::from(path);
        previous.set_extension("old.log");
        let _ = fs::rename(path, previous);
    }

    OpenOptions::new().create(true).append(true).open(path)
}

fn install(logger: Logger) {
    let level = logger.level;
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
}
