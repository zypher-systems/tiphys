//! One host per state directory.
//!
//! Whatever hosts the agent, the daemon or an app running by itself, holds
//! this lock for as long as it runs. A second one would pick up the same
//! session and take a turn the first is in the middle of for one that was cut
//! short. The lock is released when its holder ends, however it ends.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir};
use crate::{Error, Result};

/// The file the lock is held on, under the state directory.
pub const LOCK_FILE: &str = "tiphys.lock";

/// Held while a host runs. Dropping it lets another start.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// Takes the lock on `home`, or says that something else has it.
pub fn take(home: &Path) -> Result<InstanceLock> {
    ensure_dir(home, SHARED_DIR)?;
    let path = home.join(LOCK_FILE);
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", path.display()));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SHARED_FILE)
        .open(&path)
        .map_err(io)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(InstanceLock { _file: file }),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(Error::Config(format!(
            "another Tiphys is already running on {}",
            home.display()
        ))),
        Err(e) => Err(io(e.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let first = take(&home).unwrap();
        let refused = take(&home).unwrap_err();
        assert!(refused.to_string().contains("already running"), "{refused}");
        drop(first);
        take(&home).unwrap();
    }
}
