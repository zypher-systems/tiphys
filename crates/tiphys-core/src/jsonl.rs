//! Append-only JSON Lines files that survive a crash.
//!
//! Transcripts, events, the action log and the spend ledger are all one JSON
//! object per line, only ever added to. A record is on disk before `append`
//! returns. If the process dies in the middle of a write, the file ends in
//! part of a line with no newline after it: a torn tail. Reading skips it, and
//! the next append moves it aside into `<file>.torn` before writing, so one
//! bad write never poisons the records that follow.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::files::{SHARED_FILE, sync_dir};
use crate::{Error, Result};

/// Adds one record to the end of `path`, creating the file if it is missing.
/// Appends from several processes do not interleave.
pub fn append<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
    let mut line = serde_json::to_vec(record)
        .map_err(|e| Error::Io(format!("{}: cannot encode record: {e}", path.display())))?;
    line.push(b'\n');

    let is_new = !path.exists();
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(SHARED_FILE)
        .open(path)
        .map_err(io)?;
    // Held until `file` is dropped at the end of this call.
    flock(&file, FlockOperation::LockExclusive).map_err(|e| io(e.into()))?;
    set_aside_torn_tail(path, &file).map_err(io)?;
    file.write_all(&line).map_err(io)?;
    file.sync_all().map_err(io)?;
    if is_new && let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

/// Reads every complete record in `path`. A missing file has no records. A
/// torn tail is skipped; a damaged line anywhere else is an error that names
/// the line, because records after it cannot be trusted to be in order.
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    let complete = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(last_newline) => &bytes[..last_newline],
        None => &[][..],
    };
    let mut records = Vec::new();
    for (index, line) in complete.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_slice(line)
            .map_err(|e| Error::Io(format!("{}: line {}: {e}", path.display(), index + 1)))?;
        records.push(record);
    }
    Ok(records)
}

/// Where the torn tails of `path` are kept.
pub fn torn_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".torn");
    PathBuf::from(name)
}

/// If the file ends in part of a line, copies that part to the `.torn` file
/// and cuts it off, so the next record starts on a line of its own.
fn set_aside_torn_tail(path: &Path, file: &File) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    let keep = end_of_last_complete_line(file, len)?;
    if keep == len {
        return Ok(());
    }
    let mut torn = vec![0u8; (len - keep) as usize];
    file.read_exact_at(&mut torn, keep)?;
    torn.push(b'\n');
    let mut aside = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(SHARED_FILE)
        .open(torn_path(path))?;
    aside.write_all(&torn)?;
    aside.sync_all()?;
    file.set_len(keep)?;
    file.sync_all()
}

/// The offset just past the last newline, or 0 if there is none.
fn end_of_last_complete_line(file: &File, len: u64) -> std::io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut end = len;
    let mut buf = vec![0u8; CHUNK as usize];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let chunk = &mut buf[..(end - start) as usize];
        file.read_exact_at(chunk, start)?;
        if let Some(newline) = chunk.iter().rposition(|b| *b == b'\n') {
            return Ok(start + newline as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Record {
        seq: u32,
        text: String,
    }

    fn record(seq: u32, text: &str) -> Record {
        Record {
            seq,
            text: text.into(),
        }
    }

    fn add_raw(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn records_come_back_in_the_order_they_were_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        assert_eq!(read::<Record>(&path).unwrap(), []);

        append(&path, &record(1, "one")).unwrap();
        append(&path, &record(2, "two\nlines")).unwrap();
        assert_eq!(
            read::<Record>(&path).unwrap(),
            [record(1, "one"), record(2, "two\nlines")]
        );
        // One record per line, whatever the text holds.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn a_torn_tail_is_skipped_on_read_and_set_aside_on_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, &record(1, "one")).unwrap();
        add_raw(&path, b"{\"seq\":2,\"te");

        assert_eq!(read::<Record>(&path).unwrap(), [record(1, "one")]);

        append(&path, &record(3, "three")).unwrap();
        assert_eq!(
            read::<Record>(&path).unwrap(),
            [record(1, "one"), record(3, "three")]
        );
        let torn = std::fs::read_to_string(torn_path(&path)).unwrap();
        assert_eq!(torn, "{\"seq\":2,\"te\n");
    }

    #[test]
    fn a_file_that_is_all_torn_tail_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"{\"seq\":1").unwrap();
        assert_eq!(read::<Record>(&path).unwrap(), []);

        append(&path, &record(2, "two")).unwrap();
        assert_eq!(read::<Record>(&path).unwrap(), [record(2, "two")]);
    }

    #[test]
    fn a_torn_tail_longer_than_one_read_is_still_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, &record(1, "one")).unwrap();
        let mut tail = b"{\"seq\":2,\"text\":\"".to_vec();
        tail.resize(200 * 1024, b'x');
        add_raw(&path, &tail);

        append(&path, &record(3, "three")).unwrap();
        assert_eq!(
            read::<Record>(&path).unwrap(),
            [record(1, "one"), record(3, "three")]
        );
    }

    #[test]
    fn a_damaged_line_in_the_middle_is_an_error_that_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, &record(1, "one")).unwrap();
        add_raw(&path, b"not json\n");
        append(&path, &record(3, "three")).unwrap();

        let err = read::<Record>(&path).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn appends_from_many_threads_do_not_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let text = "x".repeat(8 * 1024);
        std::thread::scope(|scope| {
            for thread in 0..8 {
                let (path, text) = (&path, &text);
                scope.spawn(move || {
                    for _ in 0..25 {
                        append(path, &record(thread, text)).unwrap();
                    }
                });
            }
        });
        let records = read::<Record>(&path).unwrap();
        assert_eq!(records.len(), 200);
        assert!(records.iter().all(|r| r.text == text));
        assert!(!torn_path(&path).exists());
    }
}
