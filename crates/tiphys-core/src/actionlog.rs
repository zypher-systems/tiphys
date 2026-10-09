//! The action log: a record of every tool call, in `log/YYYY-MM.jsonl`.
//!
//! ```json
//! {"seq":7,"at":"2026-10-09T12:00:00Z","session":"0199…","audience":"terminal","tool":"write_file","class":"system","summary":"write /etc/hosts","reason":"to add a host","gate":"approved","ok":true,"prev":"9f2c…","hash":"41ab…"}
//! ```
//!
//! Every call is recorded: the ones that ran, the ones the owner refused, the
//! ones the rules refused, and the ones that made no sense. Each entry
//! carries the hash of the entry before it, and its own hash covers
//! everything else in it, so an entry that is edited or removed breaks the
//! chain from that point on. [`verify`] walks the chain.
//!
//! This is evidence for the owner, not a lock: a program running as the
//! Tiphys user could rewrite the whole file with a fresh chain. The rules
//! refuse every write to it, and the machine is the boundary.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Utc};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir};
use crate::policy::Class;
use crate::{Error, Result, jsonl};

/// The directory under the state directory that holds the log.
pub const LOG_DIR: &str = "log";
/// What the first entry of all gives as the hash before it.
const START: &str = "start";

/// How an action came to run, or not to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// It needed no asking.
    Free,
    /// The owner said yes.
    Approved,
    /// The owner said no, or nobody answered.
    Denied,
    /// The rules refused it.
    Refused,
    /// The call could not be understood, so there was nothing to judge.
    Invalid,
}

/// What is recorded about a call, before it is chained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub session: String,
    pub audience: String,
    pub tool: String,
    /// Absent when the call could not be understood.
    pub class: Option<Class>,
    pub summary: String,
    pub reason: String,
    pub gate: Gate,
    /// Whether it ran and did what was asked.
    pub ok: bool,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub session: String,
    pub audience: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<Class>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub gate: Gate,
    pub ok: bool,
    /// The hash of the entry before this one.
    pub prev: String,
    /// The hash of this entry: everything above, as it is written.
    pub hash: String,
}

impl Entry {
    /// The hash this entry should have.
    fn hashed(&self) -> String {
        let unsigned = Self {
            hash: String::new(),
            ..self.clone()
        };
        // Field order is the struct's, so the same entry always gives the
        // same bytes.
        let bytes = serde_json::to_vec(&unsigned).unwrap_or_default();
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The action log of one state directory.
#[derive(Debug, Clone)]
pub struct ActionLog {
    dir: PathBuf,
}

impl ActionLog {
    pub fn at(home: &Path) -> Self {
        Self {
            dir: home.join(LOG_DIR),
        }
    }

    /// Checks that an entry could be written right now. An action whose
    /// record cannot be kept does not run.
    pub fn ready(&self) -> Result<()> {
        self.lock().map(drop)
    }

    /// Adds an entry for `record`, chained to the one before it.
    pub fn append(&self, record: Record) -> Result<Entry> {
        self.append_at(record, Utc::now())
    }

    fn append_at(&self, record: Record, at: DateTime<Utc>) -> Result<Entry> {
        // Held across reading the last entry and writing the next, so two
        // processes cannot both chain to the same one.
        let _lock = self.lock()?;
        let last = match self.files()?.last() {
            Some(latest) => jsonl::last::<Entry>(latest)?,
            None => None,
        };
        let mut entry = Entry {
            seq: last.as_ref().map_or(1, |last| last.seq + 1),
            at,
            session: record.session,
            audience: record.audience,
            tool: record.tool,
            class: record.class,
            summary: record.summary,
            reason: record.reason,
            gate: record.gate,
            ok: record.ok,
            prev: last.map_or_else(|| START.to_string(), |last| last.hash),
            hash: String::new(),
        };
        entry.hash = entry.hashed();
        let file = self
            .dir
            .join(format!("{:04}-{:02}.jsonl", at.year(), at.month()));
        jsonl::append(&file, &entry)?;
        Ok(entry)
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for file in self.files()? {
            entries.extend(jsonl::read::<Entry>(&file)?);
        }
        Ok(entries)
    }

    /// Walks the chain. Returns how many entries hold together, or says where
    /// the chain first breaks.
    pub fn verify(&self) -> Result<u64> {
        let mut prev = START.to_string();
        let mut count = 0;
        for entry in self.entries()? {
            let broken = |what: &str| {
                Error::Io(format!(
                    "the action log is broken at entry {} ({}): {what}",
                    entry.seq,
                    entry.at.format("%Y-%m-%d %H:%M:%S")
                ))
            };
            if entry.seq != count + 1 {
                return Err(broken(&format!(
                    "it follows entry {count}, so one or more entries are missing"
                )));
            }
            if entry.prev != prev {
                return Err(broken("it does not follow from the entry before it"));
            }
            if entry.hash != entry.hashed() {
                return Err(broken("it was changed after it was written"));
            }
            prev = entry.hash;
            count += 1;
        }
        Ok(count)
    }

    /// The month files, oldest first.
    fn files(&self) -> Result<Vec<PathBuf>> {
        let listing = match std::fs::read_dir(&self.dir) {
            Ok(listing) => listing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(format!("{}: {e}", self.dir.display()))),
        };
        let mut files: Vec<PathBuf> = listing
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|ending| ending == "jsonl"))
            .collect();
        // `YYYY-MM` sorts by date.
        files.sort();
        Ok(files)
    }

    fn lock(&self) -> Result<std::fs::File> {
        ensure_dir(&self.dir, SHARED_DIR)?;
        let path = self.dir.join(".lock");
        let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(SHARED_FILE)
            .open(&path)
            .map_err(io)?;
        flock(&file, FlockOperation::LockExclusive).map_err(|e| io(e.into()))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn record(tool: &str, gate: Gate, ok: bool) -> Record {
        Record {
            session: "s1".into(),
            audience: "terminal".into(),
            tool: tool.into(),
            class: Some(Class::Change),
            summary: format!("{tool} something"),
            reason: "because".into(),
            gate,
            ok,
        }
    }

    fn filled(log: &ActionLog) {
        log.append(record("read_file", Gate::Free, true)).unwrap();
        log.append(record("write_file", Gate::Approved, true))
            .unwrap();
        log.append(record("write_file", Gate::Denied, false))
            .unwrap();
    }

    /// Rewrites one line of the log's only file.
    fn tamper(home: &Path, change: impl Fn(&mut Vec<String>)) {
        let dir = home.join(LOG_DIR);
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .unwrap();
        let mut lines: Vec<String> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        change(&mut lines);
        std::fs::write(&file, lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn entries_are_numbered_and_each_follows_from_the_one_before() {
        let home = tempfile::tempdir().unwrap();
        let log = ActionLog::at(home.path());
        assert_eq!(log.verify().unwrap(), 0);
        filled(&log);

        let entries = log.entries().unwrap();
        assert_eq!(entries.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(entries[0].prev, "start");
        assert_eq!(entries[1].prev, entries[0].hash);
        assert_eq!(entries[2].prev, entries[1].hash);
        assert_eq!((entries[2].gate, entries[2].ok), (Gate::Denied, false));
        assert_eq!(entries[0].hash.len(), 64);
        assert_eq!(log.verify().unwrap(), 3);
    }

    #[test]
    fn a_changed_removed_or_reordered_entry_breaks_the_chain_where_it_is() {
        type Tamper = fn(&mut Vec<String>);
        let cases: [(&str, Tamper, &str); 4] = [
            (
                "changed",
                |lines| lines[1] = lines[1].replace("\"gate\":\"approved\"", "\"gate\":\"free\""),
                "entry 2",
            ),
            (
                "removed from the middle",
                |lines| {
                    lines.remove(1);
                },
                "entry 3",
            ),
            (
                "removed from the start",
                |lines| {
                    lines.remove(0);
                },
                "entry 2",
            ),
            ("swapped", |lines| lines.swap(1, 2), "entry 3"),
        ];
        for (what, change, expected) in cases {
            let home = tempfile::tempdir().unwrap();
            let log = ActionLog::at(home.path());
            filled(&log);
            tamper(home.path(), change);
            let err = log.verify().unwrap_err().to_string();
            assert!(err.contains(expected), "{what}: {err}");
        }
    }

    #[test]
    fn the_chain_runs_on_from_one_month_into_the_next() {
        let home = tempfile::tempdir().unwrap();
        let log = ActionLog::at(home.path());
        let october = Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 0).unwrap();
        let november = Utc.with_ymd_and_hms(2026, 11, 1, 0, 1, 0).unwrap();
        let first = log
            .append_at(record("read_file", Gate::Free, true), october)
            .unwrap();
        let second = log
            .append_at(record("read_file", Gate::Free, true), november)
            .unwrap();

        assert_eq!((second.seq, second.prev.as_str()), (2, first.hash.as_str()));
        assert!(home.path().join("log/2026-10.jsonl").is_file());
        assert!(home.path().join("log/2026-11.jsonl").is_file());
        assert_eq!(log.verify().unwrap(), 2);
    }

    #[test]
    fn a_log_that_cannot_be_written_is_not_ready() {
        let home = tempfile::tempdir().unwrap();
        // The log directory's place is taken by a file.
        std::fs::write(home.path().join(LOG_DIR), b"").unwrap();
        let log = ActionLog::at(home.path());
        assert!(log.ready().is_err());
        assert!(log.append(record("read_file", Gate::Free, true)).is_err());

        let fine = tempfile::tempdir().unwrap();
        assert!(ActionLog::at(fine.path()).ready().is_ok());
    }
}
