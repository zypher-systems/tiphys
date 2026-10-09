//! The diagnostics log: `tiphys.log`.
//!
//! One line per entry, a UTC timestamp and a message, for working out what
//! happened after the fact. It is not the action log: nothing depends on it,
//! and a failure to write it is ignored, because diagnostics must never stop
//! the work they describe. Keys and tokens are never passed to it.
//!
//! When the file passes its size limit it is renamed to `tiphys.log.1`,
//! replacing the previous one, and a new file is started.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir};

/// The file name, under the state directory.
pub const LOG_FILE: &str = "tiphys.log";
const LIMIT: u64 = 4 * 1024 * 1024;

/// A handle on the diagnostics log. Cheap to clone and to pass around.
#[derive(Debug, Clone)]
pub struct Log {
    path: PathBuf,
    limit: u64,
}

impl Log {
    /// The log of the state directory `home`.
    pub fn at(home: &Path) -> Self {
        Self {
            path: home.join(LOG_FILE),
            limit: LIMIT,
        }
    }

    /// Adds an entry stamped with the current time.
    pub fn line(&self, message: &str) {
        self.line_at(Utc::now(), message);
    }

    /// Adds an entry stamped with `when`.
    pub fn line_at(&self, when: DateTime<Utc>, message: &str) {
        let _ = self.write(when, message);
    }

    fn write(&self, when: DateTime<Utc>, message: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            ensure_dir(dir, SHARED_DIR).map_err(std::io::Error::other)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= self.limit) {
            std::fs::rename(&self.path, self.rotated())?;
        }
        // An entry is one line, so a message that spans several is flattened.
        let entry = format!(
            "{} {}\n",
            when.to_rfc3339_opts(SecondsFormat::Millis, true),
            message.replace('\\', "\\\\").replace('\n', "\\n")
        );
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(SHARED_FILE)
            .open(&self.path)?
            .write_all(entry.as_bytes())
    }

    fn rotated(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn noon() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
    }

    #[test]
    fn an_entry_is_one_stamped_line() {
        let home = tempfile::tempdir().unwrap();
        let log = Log::at(home.path());
        log.line_at(noon(), "daemon started");
        log.line_at(noon(), "two\nlines and a \\ slash");

        let text = std::fs::read_to_string(home.path().join(LOG_FILE)).unwrap();
        assert_eq!(
            text,
            "2026-10-09T12:00:00.000Z daemon started\n\
             2026-10-09T12:00:00.000Z two\\nlines and a \\\\ slash\n"
        );
    }

    #[test]
    fn a_full_log_is_moved_aside_and_a_new_one_started() {
        let home = tempfile::tempdir().unwrap();
        let log = Log {
            limit: 40,
            ..Log::at(home.path())
        };
        log.line_at(noon(), "first entry, long enough to fill it");
        log.line_at(noon(), "second");

        let current = std::fs::read_to_string(home.path().join(LOG_FILE)).unwrap();
        let rotated = std::fs::read_to_string(home.path().join("tiphys.log.1")).unwrap();
        assert!(current.ends_with("second\n") && current.lines().count() == 1);
        assert!(rotated.contains("first entry"));
    }

    #[test]
    fn a_log_that_cannot_be_written_stops_nothing() {
        let home = tempfile::tempdir().unwrap();
        // The state directory's place is taken by a file.
        let blocked = home.path().join("state");
        std::fs::write(&blocked, b"").unwrap();
        Log::at(&blocked).line("lost");
    }
}
