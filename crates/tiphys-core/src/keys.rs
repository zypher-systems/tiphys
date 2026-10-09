//! The key store: `keys/<name>`, one secret per file.
//!
//! A key reaches this module from one place, the masked field in the app. It
//! is written to a file only the Tiphys user can read, inside a directory
//! only that user can enter, and it is never handed back for display. No
//! command takes a key as an argument, so none ends up in shell history or
//! the process list.
//!
//! A connection may instead name an environment variable (`env_key`), for a
//! server that starts with nobody present. When that variable is set it is
//! used; otherwise the stored key is.
//!
//! A connection's key is stored under the connection's name. A key that is
//! not a connection's, such as a chat bot's token, is stored under a name
//! that starts with `_`, which no connection can have.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::valid_name;
use crate::files::{PRIVATE_DIR, PRIVATE_FILE, ensure_dir_with_mode, write_atomic};
use crate::{Error, Result};

/// The directory under the state directory that holds the keys.
pub const KEYS_DIR: &str = "keys";
/// The name the Telegram bot's token is stored under.
pub const TELEGRAM: &str = "_telegram";

/// A name a key can be stored under: a connection's name, or one of
/// Tiphys's own, which starts with `_`.
fn valid_key_name(name: &str) -> Result<()> {
    valid_name(name.strip_prefix('_').unwrap_or(name))
}

/// A key or token. It does not print: `Debug` shows a placeholder, and there
/// is no `Display`, so a secret cannot slip into a log line or an error by
/// way of a format string.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps what was typed, without surrounding whitespace. Nothing typed is
    /// no secret.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self(text.to_string()))
    }

    /// The secret itself, for the request header it belongs in.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A secret is written out in one place only: a frame from the app to the
/// daemon, on the local socket, when the owner has just typed it. Nothing
/// that is kept, logged or sent to a model holds one.
impl serde::Serialize for Secret {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(&text).ok_or_else(|| serde::de::Error::custom("an empty secret"))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Stores `secret` under `name`, replacing any earlier one.
pub fn store(home: &Path, name: &str, secret: &Secret) -> Result<()> {
    valid_key_name(name)?;
    let dir = home.join(KEYS_DIR);
    ensure_dir_with_mode(&dir, PRIVATE_DIR)?;
    write_atomic(&dir.join(name), secret.expose().as_bytes(), PRIVATE_FILE)
}

/// Whether a key is stored under `name`. This is all the app shows of one.
pub fn is_stored(home: &Path, name: &str) -> bool {
    valid_key_name(name).is_ok() && path(home, name).is_file()
}

/// Deletes the key stored under `name`. Returns whether there was one.
pub fn remove(home: &Path, name: &str) -> Result<bool> {
    valid_key_name(name)?;
    let path = path(home, name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(format!("{}: {e}", path.display()))),
    }
}

/// The key for a connection: its `env_key` variable if that is set and not
/// empty, otherwise the stored key. `None` means the connection has no key,
/// which is right for a local server.
pub fn resolve(home: &Path, name: &str, env_key: Option<&str>) -> Result<Option<Secret>> {
    resolve_with(home, name, env_key, |variable| std::env::var(variable).ok())
}

/// [`resolve`] with the environment passed in.
pub fn resolve_with(
    home: &Path,
    name: &str,
    env_key: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Secret>> {
    if let Some(secret) = env_key.and_then(env).and_then(|v| Secret::new(&v)) {
        return Ok(Some(secret));
    }
    read(home, name)
}

fn read(home: &Path, name: &str) -> Result<Option<Secret>> {
    valid_key_name(name)?;
    let path = path(home, name);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    // Tiphys only ever creates this file private. If it is open to others now,
    // something else changed it, and the key may already have been read.
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Config(format!(
            "{} can be read by other users; enter the key again in the app, or run chmod 600 on it",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    Ok(Secret::new(&text))
}

fn path(home: &Path, name: &str) -> PathBuf {
    home.join(KEYS_DIR).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;

    fn secret(text: &str) -> Secret {
        Secret::new(text).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_stored_key_is_private_and_comes_back_as_typed() {
        let home = tempfile::tempdir().unwrap();
        assert!(!is_stored(home.path(), "work"));
        store(home.path(), "work", &secret("  sk-one\n")).unwrap();

        assert!(is_stored(home.path(), "work"));
        assert_eq!(mode_of(&home.path().join("keys")), 0o700);
        assert_eq!(mode_of(&home.path().join("keys/work")), 0o600);
        let back = resolve_with(home.path(), "work", None, |_| None).unwrap();
        assert_eq!(back.unwrap().expose(), "sk-one");
    }

    #[test]
    fn keys_resolve_from_the_variable_first_and_an_empty_one_is_unset() {
        let home = tempfile::tempdir().unwrap();
        store(home.path(), "work", &secret("stored")).unwrap();
        let cases = [
            (Some("WORK_KEY"), Some("from-env"), Some("from-env")),
            (Some("WORK_KEY"), Some("  "), Some("stored")),
            (Some("WORK_KEY"), None, Some("stored")),
            (None, Some("from-env"), Some("stored")),
        ];
        for (env_key, value, expected) in cases {
            let got = resolve_with(home.path(), "work", env_key, |name| {
                assert_eq!(name, "WORK_KEY");
                value.map(str::to_string)
            })
            .unwrap();
            assert_eq!(
                got.as_ref().map(Secret::expose),
                expected,
                "{env_key:?} {value:?}"
            );
        }
        // No variable and nothing stored: a keyless connection.
        let none = resolve_with(home.path(), "local", None, |_| None).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn a_key_file_others_can_read_is_refused() {
        let home = tempfile::tempdir().unwrap();
        store(home.path(), "work", &secret("sk-one")).unwrap();
        let path = home.path().join("keys/work");
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        let err = resolve_with(home.path(), "work", None, |_| None).unwrap_err();
        assert!(matches!(err, Error::Config(ref m) if m.contains("other users")));
        assert!(!err.to_string().contains("sk-one"));
    }

    #[test]
    fn storing_again_replaces_the_key_and_removing_it_leaves_none() {
        let home = tempfile::tempdir().unwrap();
        store(home.path(), "work", &secret("one")).unwrap();
        store(home.path(), "work", &secret("two")).unwrap();
        let back = resolve_with(home.path(), "work", None, |_| None).unwrap();
        assert_eq!(back.unwrap().expose(), "two");

        assert!(remove(home.path(), "work").unwrap());
        assert!(!remove(home.path(), "work").unwrap());
        assert!(!is_stored(home.path(), "work"));
    }

    #[test]
    fn a_name_cannot_reach_outside_the_keys_directory() {
        let home = tempfile::tempdir().unwrap();
        for name in ["../config", "a/b", "", ".hidden"] {
            assert!(store(home.path(), name, &secret("x")).is_err(), "{name}");
            assert!(
                resolve_with(home.path(), name, None, |_| None).is_err(),
                "{name}"
            );
            assert!(!is_stored(home.path(), name), "{name}");
        }
        assert!(!home.path().join("config").exists());
    }

    #[test]
    fn a_key_of_tiphys_own_has_a_name_no_connection_can_take() {
        let home = tempfile::tempdir().unwrap();
        assert!(valid_name(TELEGRAM).is_err());
        store(home.path(), TELEGRAM, &secret("123:bot")).unwrap();
        // A connection that happens to be called telegram keeps its own key.
        store(home.path(), "telegram", &secret("sk-model")).unwrap();
        assert_eq!(
            resolve(home.path(), TELEGRAM, None).unwrap(),
            Some(secret("123:bot"))
        );
        assert_eq!(
            resolve(home.path(), "telegram", None).unwrap(),
            Some(secret("sk-model"))
        );
        // One underscore, and still nothing that leaves the directory.
        for name in ["__telegram", "_", "_../x", "_a/b"] {
            assert!(store(home.path(), name, &secret("x")).is_err(), "{name}");
        }
    }

    #[test]
    fn a_secret_does_not_print() {
        assert_eq!(format!("{:?}", secret("sk-live-123")), "Secret(***)");
        assert!(Secret::new(" \n").is_none());
    }
}
