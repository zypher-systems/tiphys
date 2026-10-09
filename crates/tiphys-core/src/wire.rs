//! How a client and the daemon talk over the socket.
//!
//! Each side writes one JSON object per line. A client opens with a hello
//! that says which version of the protocol it speaks and who it is talking
//! as; the daemon answers with its own hello, or refuses. After that the
//! client sends requests and the daemon sends events.
//!
//! ```text
//! → {"frame":"hello","protocol":2,"version":"0.1.0","audience":"terminal"}
//! ← {"frame":"hello","protocol":2,"version":"0.1.0"}
//! ← {"frame":"event","event":{"kind":"state","connections":[]}}
//! → {"frame":"request","request":{"kind":"prompt","text":"how full is the disk?"}}
//! ← {"frame":"event","event":{"kind":"user_message","text":"how full is the disk?"}}
//! ```
//!
//! The socket is a Unix socket on the machine itself. It is never a network
//! port.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::proto::{Event, Request};

/// The version of this protocol. A client and a daemon that differ do not
/// try to understand each other.
pub const PROTOCOL: u32 = 2;

/// The variable that says where the socket is, for both sides.
pub const SOCKET_ENV: &str = "TIPHYS_SOCKET";
/// Where a daemon installed as a system service listens.
pub const SYSTEM_SOCKET: &str = "/run/tiphys/tiphys.sock";
/// The socket's name, wherever it is.
pub const SOCKET_FILE: &str = "tiphys.sock";

/// The conversation the owner has at a terminal.
pub const TERMINAL: &str = "terminal";
/// Runs made with `tiphys -p`, which are their own conversation.
pub const ONESHOT: &str = "oneshot";
/// The audiences a socket client may talk as.
pub const AUDIENCES: &[&str] = &[TERMINAL, ONESHOT];

/// What a client writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello {
        protocol: u32,
        version: String,
        audience: String,
    },
    Request {
        request: Request,
    },
}

/// What the daemon writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello {
        protocol: u32,
        version: String,
    },
    /// The daemon will not serve this client. Nothing follows.
    Refused {
        message: String,
    },
    Event {
        event: Event,
    },
}

/// A frame as one line, newline included.
pub fn line<T: Serialize>(frame: &T) -> String {
    // These types hold nothing a JSON encoder can fail on.
    let mut line = serde_json::to_string(frame).unwrap_or_default();
    line.push('\n');
    line
}

/// An event frame as one line, without copying the event first.
pub fn event_line(event: &Event) -> String {
    #[derive(Serialize)]
    #[serde(tag = "frame", rename_all = "snake_case")]
    enum Out<'a> {
        Event { event: &'a Event },
    }
    line(&Out::Event { event })
}

/// Where a daemon for the state directory `home` listens: where it is told
/// to, or under the runtime directory systemd gives a service, or beside its
/// state when it is run by hand.
pub fn listen_path(home: &Path) -> PathBuf {
    listen_path_with(home, |name| std::env::var_os(name))
}

/// [`listen_path`] with the environment passed in.
pub fn listen_path_with(home: &Path, env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(path) = set(SOCKET_ENV) {
        return PathBuf::from(path);
    }
    match set("RUNTIME_DIRECTORY") {
        // systemd may list several directories, separated by colons.
        Some(dirs) => {
            let first = dirs
                .to_string_lossy()
                .split(':')
                .next()
                .unwrap_or_default()
                .to_string();
            PathBuf::from(first).join(SOCKET_FILE)
        }
        None => home.join(SOCKET_FILE),
    }
}

/// Where a client should look for a daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Daemon {
    /// A daemon is expected here. If it does not answer, that is an error to
    /// report, not a reason to carry on without it.
    Expected(PathBuf),
    /// A socket was left beside the state directory. Something may be
    /// listening, or it may be left over from a daemon that is gone.
    Perhaps(PathBuf),
    /// No sign of one.
    None,
}

/// Looks for a daemon serving the owner of `home`.
pub fn find(home: &Path) -> Daemon {
    find_with(
        home,
        |name| std::env::var_os(name),
        Path::new(SYSTEM_SOCKET),
    )
}

/// [`find`] with the environment and the system socket's path passed in.
pub fn find_with(home: &Path, env: impl Fn(&str) -> Option<OsString>, system: &Path) -> Daemon {
    if let Some(path) = env(SOCKET_ENV).filter(|value| !value.is_empty()) {
        return Daemon::Expected(PathBuf::from(path));
    }
    if system.exists() {
        return Daemon::Expected(system.to_path_buf());
    }
    let beside = home.join(SOCKET_FILE);
    if beside.exists() {
        Daemon::Perhaps(beside)
    } else {
        Daemon::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_one_tagged_line_each_and_come_back_the_same() {
        let hello = ClientFrame::Hello {
            protocol: PROTOCOL,
            version: "0.1.0".into(),
            audience: TERMINAL.into(),
        };
        assert_eq!(
            line(&hello),
            "{\"frame\":\"hello\",\"protocol\":2,\"version\":\"0.1.0\",\"audience\":\"terminal\"}\n"
        );
        let request = ClientFrame::Request {
            request: Request::Prompt {
                text: "two\nlines".into(),
            },
        };
        let written = line(&request);
        assert_eq!(written.matches('\n').count(), 1);
        assert_eq!(
            serde_json::from_str::<ClientFrame>(&written).unwrap(),
            request
        );

        let event = Event::Notice { text: "hi".into() };
        let written = event_line(&event);
        assert_eq!(
            written,
            "{\"frame\":\"event\",\"event\":{\"kind\":\"notice\",\"text\":\"hi\"}}\n"
        );
        assert_eq!(
            serde_json::from_str::<ServerFrame>(&written).unwrap(),
            ServerFrame::Event { event }
        );
    }

    #[test]
    fn a_daemon_listens_where_it_is_told_then_where_systemd_says_then_beside_its_state() {
        let home = Path::new("/var/lib/tiphys/state");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(listen_path_with(home, env(&[])), home.join("tiphys.sock"));
        assert_eq!(
            listen_path_with(home, env(&[("RUNTIME_DIRECTORY", "/run/tiphys")])),
            Path::new("/run/tiphys/tiphys.sock")
        );
        assert_eq!(
            listen_path_with(
                home,
                env(&[("RUNTIME_DIRECTORY", "/run/tiphys:/run/other")])
            ),
            Path::new("/run/tiphys/tiphys.sock")
        );
        assert_eq!(
            listen_path_with(
                home,
                env(&[
                    ("RUNTIME_DIRECTORY", "/run/tiphys"),
                    ("TIPHYS_SOCKET", "/tmp/t.sock")
                ])
            ),
            Path::new("/tmp/t.sock")
        );
        assert_eq!(
            listen_path_with(home, env(&[("TIPHYS_SOCKET", "")])),
            home.join("tiphys.sock")
        );
    }

    #[test]
    fn a_client_looks_where_it_is_told_then_for_the_service_then_beside_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let system = dir.path().join("run/tiphys.sock");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(system.parent().unwrap()).unwrap();
        let none = |_: &str| None;
        assert_eq!(find_with(&home, none, &system), Daemon::None);

        std::fs::write(home.join("tiphys.sock"), b"").unwrap();
        assert_eq!(
            find_with(&home, none, &system),
            Daemon::Perhaps(home.join("tiphys.sock"))
        );

        std::fs::write(&system, b"").unwrap();
        assert_eq!(
            find_with(&home, none, &system),
            Daemon::Expected(system.clone())
        );

        let told = |_: &str| Some(OsString::from("/somewhere/else.sock"));
        assert_eq!(
            find_with(&home, told, &system),
            Daemon::Expected("/somewhere/else.sock".into())
        );
    }
}
