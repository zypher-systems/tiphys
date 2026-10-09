//! Writing files so that a crash never leaves half of one.
//!
//! A whole-file write goes to a temporary name in the same directory, is
//! flushed to disk, and is renamed over the target. The file gets its
//! permissions when it is created, so a private file is never readable by
//! anyone else, not even for a moment.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Readable and writable by the owner only.
pub const PRIVATE_FILE: u32 = 0o600;
/// Readable by the owner and the owner's group.
pub const SHARED_FILE: u32 = 0o640;
/// A directory only the owner can enter.
pub const PRIVATE_DIR: u32 = 0o700;
/// A directory the owner's group can read.
pub const SHARED_DIR: u32 = 0o750;

/// Creates `dir` and its parents if they are missing. An existing directory is
/// left as it is, permissions included.
pub fn ensure_dir(dir: &Path, mode: u32) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .map_err(|e| Error::Io(format!("{}: {e}", dir.display())))
}

/// Creates `dir` if it is missing and sets its permissions to `mode` either
/// way. For directories whose privacy matters, such as `keys/`.
pub fn ensure_dir_with_mode(dir: &Path, mode: u32) -> Result<()> {
    ensure_dir(dir, mode)?;
    fs::set_permissions(dir, Permissions::from_mode(mode))
        .map_err(|e| Error::Io(format!("{}: {e}", dir.display())))
}

/// Replaces `path` with `bytes`, or creates it. Readers see the old content or
/// the new, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::Io(format!("{}: no parent directory", path.display())))?;
    let tmp = temp_name(path)?;
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));

    // A leftover from a crashed write would make `create_new` fail.
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(io(e));
    }
    sync_dir(dir);
    Ok(())
}

/// Flushes a directory, so a rename or a new file in it survives a power cut.
/// Some filesystems refuse to sync a directory; that is not worth failing a
/// write that has already succeeded.
pub fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

fn temp_name(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::Io(format!("{}: not a file path", path.display())))?;
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(name);
    tmp.push(format!(".tmp.{}", std::process::id()));
    Ok(path.with_file_name(tmp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn an_atomic_write_creates_the_file_with_the_mode_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic(&path, b"one", PRIVATE_FILE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn an_atomic_write_replaces_what_was_there_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        write_atomic(&path, b"old", SHARED_FILE).unwrap();
        write_atomic(&path, b"new", SHARED_FILE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["settings.toml"]);
    }

    #[test]
    fn a_leftover_temp_file_does_not_block_the_next_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        fs::write(temp_name(&path).unwrap(), b"half").unwrap();
        write_atomic(&path, b"{}", SHARED_FILE).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
    }

    #[test]
    fn a_write_into_a_missing_directory_fails_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("file");
        let err = write_atomic(&path, b"x", SHARED_FILE).unwrap_err();
        assert!(matches!(err, Error::Io(ref m) if m.contains("missing/file")));
    }

    #[test]
    fn a_private_directory_is_tightened_even_when_it_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let keys = dir.path().join("keys");
        ensure_dir(&keys, 0o755).unwrap();
        fs::set_permissions(&keys, Permissions::from_mode(0o755)).unwrap();
        ensure_dir_with_mode(&keys, PRIVATE_DIR).unwrap();
        assert_eq!(mode_of(&keys), 0o700);
    }
}
