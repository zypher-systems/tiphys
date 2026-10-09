//! What an action is allowed to be.
//!
//! Every action a tool plans is given one [`Class`] before anything runs.
//! The class decides whether it runs, asks, or is refused. So far there are
//! only reads, and one rule about them: nothing reads the key store.

use std::path::{Component, Path, PathBuf};

use crate::keys::KEYS_DIR;

/// How much an action changes, and so who has to agree to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Reads. Runs without asking.
    Observe,
    /// Changes made as the Tiphys user.
    Change,
    /// Needs root, or changes the system.
    System,
    /// Refused, whoever asks.
    Never,
}

/// The class of reading `path`. A path inside the key store is refused, by
/// any spelling of it: through `..`, through a symlink, or as the directory
/// itself.
pub fn read_class(path: &Path, state: &Path) -> Class {
    if is_inside(path, &state.join(KEYS_DIR)) {
        Class::Never
    } else {
        Class::Observe
    }
}

/// Whether `path` is `dir` or something under it, once both are resolved.
pub fn is_inside(path: &Path, dir: &Path) -> bool {
    resolve(path).starts_with(resolve(dir))
}

/// The real location of `path`: symlinks followed where the path exists, and
/// `.` and `..` worked out by hand for the part that does not.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    // Resolve the longest leading part that exists, then apply the rest to it.
    let parts: Vec<Component> = path.components().collect();
    for split in (1..parts.len()).rev() {
        let existing: PathBuf = parts[..split].iter().collect();
        let Ok(mut resolved) = existing.canonicalize() else {
            continue;
        };
        for part in &parts[split..] {
            match part {
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => resolved.push(name),
                _ => {}
            }
        }
        return resolved;
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_store_cannot_be_read_by_any_spelling_of_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::create_dir_all(state.join("sessions")).unwrap();
        std::fs::write(state.join("keys/work"), "sk").unwrap();
        std::fs::write(state.join("config.toml"), "").unwrap();
        let link = dir.path().join("innocent");
        std::os::unix::fs::symlink(state.join("keys"), &link).unwrap();
        let file_link = dir.path().join("notes.txt");
        std::os::unix::fs::symlink(state.join("keys/work"), &file_link).unwrap();

        let refused = [
            state.join("keys"),
            state.join("keys/work"),
            state.join("keys/not-there-yet"),
            state.join("sessions/../keys/work"),
            state.join("keys/./work"),
            link.clone(),
            link.join("work"),
            file_link,
        ];
        for path in refused {
            assert_eq!(
                read_class(&path, &state),
                Class::Never,
                "{}",
                path.display()
            );
        }
        let allowed = [
            state.clone(),
            state.join("config.toml"),
            state.join("sessions"),
            state.join("keys-backup-notes.txt"),
            dir.path().join("elsewhere/keys/work"),
        ];
        for path in allowed {
            assert_eq!(
                read_class(&path, &state),
                Class::Observe,
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn classes_order_from_harmless_to_refused() {
        assert!(Class::Observe < Class::Change);
        assert!(Class::Change < Class::System);
        assert!(Class::System < Class::Never);
    }
}
