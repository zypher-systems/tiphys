//! What reading or writing a path is allowed to be.
//!
//! A path is judged where it really points: symlinks are followed and `..`
//! is worked out before any rule is applied, so a rule about a place holds
//! for every way of spelling it.
//!
//! | Path | Read | Write |
//! | --- | --- | --- |
//! | The key store | never | never |
//! | The rest of Tiphys's state directory | runs | never |
//! | The Tiphys binary, its unit, its sudoers file | runs | never |
//! | A file that usually holds a secret | asks | asks |
//! | The Tiphys user's home, `/tmp`, `/var/tmp` | runs | runs |
//! | Anywhere else | runs | asks |

use std::path::{Component, Path, PathBuf};

use super::Verdict;
use crate::keys::KEYS_DIR;

/// The places the rules are about.
#[derive(Debug, Clone, Copy)]
pub struct Places<'a> {
    /// Tiphys's state directory.
    pub state: &'a Path,
    /// The home of the user Tiphys runs as.
    pub home: &'a Path,
}

/// Files that belong to the installation. Changing one could switch off the
/// agent or the rules it runs under.
const INSTALLED: &[&str] = &[
    "/usr/local/bin/tiphys",
    "/usr/bin/tiphys",
    "/etc/systemd/system/tiphys.service",
    "/etc/sudoers.d/tiphys",
];

/// Scratch space any user may write to.
const SCRATCH: &[&str] = &["/tmp", "/var/tmp"];

/// File names that hold a secret wherever they are.
const SECRET_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".netrc",
    ".pgpass",
    ".env",
    "shadow",
    "gshadow",
    "sudoers",
    "credentials",
];

/// File endings that mean a private key or a keystore.
const SECRET_ENDINGS: &[&str] = &["pem", "key", "p12", "pfx", "kdbx", "jks"];

/// Names in `~/.ssh` that are not secrets.
const SSH_PUBLIC: &[&str] = &["known_hosts", "config", "authorized_keys"];

/// The verdict on reading `path`.
pub fn read(path: &Path, places: Places) -> Verdict {
    let real = resolve(path);
    if real.starts_with(resolve(&places.state.join(KEYS_DIR))) {
        return Verdict::never("this is Tiphys's key store, which is never read");
    }
    if is_secret(&real, places) {
        return Verdict::system(format!(
            "{} usually holds a secret, and reading it sends it to the model provider",
            real.display()
        ));
    }
    Verdict::observe()
}

/// The verdict on creating, changing or deleting `path`.
pub fn write(path: &Path, places: Places) -> Verdict {
    let real = resolve(path);
    if real.starts_with(resolve(places.state)) {
        return Verdict::never(
            "this is inside Tiphys's own state directory, which only Tiphys itself changes",
        );
    }
    if INSTALLED
        .iter()
        .any(|installed| real == Path::new(installed))
    {
        return Verdict::never(format!(
            "{} is part of the Tiphys installation",
            real.display()
        ));
    }
    if is_secret(&real, places) {
        return Verdict::system(format!("{} usually holds a secret", real.display()));
    }
    let own = real.starts_with(resolve(places.home))
        || SCRATCH.iter().any(|scratch| real.starts_with(scratch));
    if own {
        Verdict::change()
    } else {
        Verdict::system(format!("{} is outside Tiphys's own home", real.display()))
    }
}

/// Whether `path` is a file that usually holds a secret. What is already in
/// such a file is not shown in a preview, which is kept with the session.
pub fn holds_secret(path: &Path, places: Places) -> bool {
    is_secret(&resolve(path), places)
}

/// Whether a file usually holds a secret, by where it is and what it is
/// called. This reads no file; it is a judgement from the name alone.
fn is_secret(real: &Path, places: Places) -> bool {
    let name = real
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ending = name.rsplit_once('.').map(|(_, ending)| ending);
    let home = resolve(places.home);
    if real.starts_with(home.join(".ssh")) {
        return !(ending == Some("pub") || SSH_PUBLIC.contains(&name.as_str()));
    }
    if real.starts_with(home.join(".gnupg")) || real.starts_with("/etc/sudoers.d") {
        return true;
    }
    SECRET_NAMES.contains(&name.as_str())
        || name.starts_with(".env.")
        || ending.is_some_and(|ending| SECRET_ENDINGS.contains(&ending))
}

/// The real location of `path`: symlinks followed where the path exists, and
/// `.` and `..` worked out by hand for the part that does not.
pub fn resolve(path: &Path) -> PathBuf {
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
    use crate::policy::Class;

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let fixture = Self { dir };
            for sub in [
                "home/.tiphys/keys",
                "home/.tiphys/sessions",
                "home/.ssh",
                "home/work",
                "elsewhere",
            ] {
                std::fs::create_dir_all(fixture.path(sub)).unwrap();
            }
            std::fs::write(fixture.path("home/.tiphys/keys/work"), "sk").unwrap();
            std::fs::write(fixture.path("home/.tiphys/config.toml"), "").unwrap();
            fixture
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.path().join(relative)
        }

        fn read(&self, path: &Path) -> Class {
            let (state, home) = (self.path("home/.tiphys"), self.path("home"));
            read(
                path,
                Places {
                    state: &state,
                    home: &home,
                },
            )
            .class
        }

        fn write(&self, path: &Path) -> Class {
            let (state, home) = (self.path("home/.tiphys"), self.path("home"));
            write(
                path,
                Places {
                    state: &state,
                    home: &home,
                },
            )
            .class
        }
    }

    #[test]
    fn the_key_store_cannot_be_read_by_any_spelling_of_its_path() {
        let f = Fixture::new();
        let link = f.path("innocent");
        std::os::unix::fs::symlink(f.path("home/.tiphys/keys"), &link).unwrap();
        let file_link = f.path("home/work/notes.txt");
        std::os::unix::fs::symlink(f.path("home/.tiphys/keys/work"), &file_link).unwrap();

        let refused = [
            f.path("home/.tiphys/keys"),
            f.path("home/.tiphys/keys/work"),
            f.path("home/.tiphys/keys/not-there-yet"),
            f.path("home/.tiphys/sessions/../keys/work"),
            f.path("home/.tiphys/keys/./work"),
            link.clone(),
            link.join("work"),
            file_link,
        ];
        for path in refused {
            assert_eq!(f.read(&path), Class::Never, "{}", path.display());
        }
        let allowed = [
            f.path("home/.tiphys"),
            f.path("home/.tiphys/config.toml"),
            f.path("home/.tiphys/sessions"),
            f.path("home/.tiphys/keys-backup-notes.txt"),
            f.path("elsewhere/keys/work"),
        ];
        for path in allowed {
            assert_eq!(f.read(&path), Class::Observe, "{}", path.display());
        }
    }

    #[test]
    fn a_write_runs_in_the_agents_own_places_and_asks_everywhere_else() {
        // A home that is not under a scratch directory, as the test's own
        // temporary one is. Nothing here has to exist.
        let home = Path::new("/home/tiphys-test-user");
        let state = home.join(".tiphys");
        let places = Places {
            state: &state,
            home,
        };
        let cases = [
            ("/home/tiphys-test-user/work/notes.txt", Class::Change),
            ("/home/tiphys-test-user/new-dir/deep/file", Class::Change),
            ("/tmp/tiphys-scratch.txt", Class::Change),
            ("/var/tmp/x", Class::Change),
            ("/etc/hosts", Class::System),
            ("/usr/local/bin/tool", Class::System),
            ("/srv/site/index.html", Class::System),
            ("/home/someone-else/.bashrc", Class::System),
            ("/home/tiphys-test-user-2/file", Class::System),
            // Out of home by way of `..`.
            (
                "/home/tiphys-test-user/work/../../someone-else/file",
                Class::System,
            ),
            ("/tmp/../etc/hosts", Class::System),
        ];
        for (path, expected) in cases {
            assert_eq!(write(Path::new(path), places).class, expected, "{path}");
        }
    }

    #[test]
    fn tiphys_own_files_and_its_installation_are_never_written() {
        let f = Fixture::new();
        let link = f.path("home/work/state");
        std::os::unix::fs::symlink(f.path("home/.tiphys"), &link).unwrap();
        let refused = [
            f.path("home/.tiphys/config.toml"),
            f.path("home/.tiphys/settings.toml"),
            f.path("home/.tiphys/keys/work"),
            f.path("home/.tiphys/log/2026-10.jsonl"),
            f.path("home/.tiphys/sessions/x/transcript.jsonl"),
            f.path("home/.tiphys"),
            link.join("config.toml"),
            PathBuf::from("/usr/local/bin/tiphys"),
            PathBuf::from("/etc/systemd/system/tiphys.service"),
            PathBuf::from("/etc/sudoers.d/tiphys"),
        ];
        for path in refused {
            assert_eq!(f.write(&path), Class::Never, "{}", path.display());
        }
        // A neighbour with a similar name is just a file in home.
        assert_eq!(f.write(&f.path("home/.tiphys-notes")), Class::Change);
    }

    #[test]
    fn files_that_usually_hold_a_secret_ask_before_they_are_read_or_written() {
        let f = Fixture::new();
        let secrets = [
            f.path("home/.ssh/id_ed25519"),
            f.path("home/.ssh/some-key"),
            f.path("home/.gnupg/private-keys-v1.d/x.key"),
            f.path("home/work/.env"),
            f.path("home/work/.env.production"),
            f.path("home/work/server.pem"),
            f.path("home/work/TLS.KEY"),
            f.path("home/.aws/credentials"),
            f.path("home/.netrc"),
            PathBuf::from("/etc/shadow"),
            PathBuf::from("/etc/sudoers"),
            PathBuf::from("/etc/sudoers.d/other"),
        ];
        for path in secrets {
            assert_eq!(f.read(&path), Class::System, "read {}", path.display());
            assert_eq!(f.write(&path), Class::System, "write {}", path.display());
        }
        let plain = [
            f.path("home/.ssh/id_ed25519.pub"),
            f.path("home/.ssh/known_hosts"),
            f.path("home/.ssh/config"),
            f.path("home/work/keyboard.txt"),
            f.path("home/work/environment.md"),
            PathBuf::from("/etc/hosts"),
        ];
        for path in plain {
            assert_eq!(f.read(&path), Class::Observe, "read {}", path.display());
        }
    }

    #[test]
    fn a_verdict_that_asks_or_refuses_says_why() {
        let f = Fixture::new();
        let (state, home) = (f.path("home/.tiphys"), f.path("home"));
        let places = Places {
            state: &state,
            home: &home,
        };
        assert!(
            read(&f.path("home/.tiphys/keys/work"), places)
                .why
                .contains("key store")
        );
        assert!(
            read(&f.path("home/.ssh/id_rsa"), places)
                .why
                .contains("sends it to the model provider")
        );
        assert!(
            write(Path::new("/etc/hosts"), places)
                .why
                .contains("outside Tiphys's own home")
        );
        assert!(
            write(&f.path("home/.tiphys/config.toml"), places)
                .why
                .contains("state directory")
        );
        assert!(write(&f.path("home/work/a.txt"), places).why.is_empty());
    }
}
