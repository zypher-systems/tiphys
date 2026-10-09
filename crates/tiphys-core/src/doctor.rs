//! `tiphys doctor`: is this installation in working order?
//!
//! Each check looks at one thing and says what it found. Nothing is changed
//! and no key is read: a key is only looked at from the outside, for whether
//! it is there and who can read it. The live check is the one exception to
//! "nothing is changed": it makes a real, paid call to the model.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::actionlog::ActionLog;
use crate::config::{self, Config};
use crate::keys::{self, KEYS_DIR};
use crate::llm::{Connect, check};

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn pass(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: true,
        detail: detail.into(),
    }
}

fn fail(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: false,
        detail: detail.into(),
    }
}

/// Runs every check that needs no network.
pub fn run(home: &Path) -> Vec<Check> {
    let mut checks = vec![state_directory(home)];
    let config = match config::load_at(home) {
        Ok(config) => {
            checks.push(pass(
                "configuration",
                "config.toml and settings.toml read cleanly",
            ));
            Some(config)
        }
        Err(e) => {
            checks.push(fail("configuration", e.to_string()));
            None
        }
    };
    if let Some(config) = &config {
        checks.extend(connections(home, config));
        checks.push(telegram(home, config));
    }
    checks.push(key_store(home));
    checks.push(action_log(home));
    checks.push(bash());
    checks
}

/// Makes a real tool-call round trip on the connection new sessions use.
pub async fn live(home: &Path, connect: &dyn Connect) -> Check {
    const NAME: &str = "live round trip";
    let config = match config::load_at(home) {
        Ok(config) => config,
        Err(e) => return fail(NAME, e.to_string()),
    };
    let Some((name, connection)) = config.starting_connection() else {
        return fail(NAME, "there is no connection to try");
    };
    let Some(model) = &connection.model else {
        return fail(NAME, format!("connection `{name}` has no model chosen"));
    };
    let provider = keys::resolve(home, name, connection.env_key.as_deref())
        .and_then(|key| connect.provider(connection, key.as_ref()));
    let provider = match provider {
        Ok(provider) => provider,
        Err(e) => return fail(NAME, e.to_string()),
    };
    match check::tool_round_trip(provider.as_ref(), model).await {
        Ok(()) => pass(
            NAME,
            format!("{model} on `{name}` called a tool and read its result"),
        ),
        Err(e) => fail(NAME, format!("{model} on `{name}`: {e}")),
    }
}

fn state_directory(home: &Path) -> Check {
    const NAME: &str = "state directory";
    match std::fs::metadata(home) {
        Ok(metadata) if metadata.is_dir() => pass(NAME, home.display().to_string()),
        Ok(_) => fail(NAME, format!("{} is not a directory", home.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => pass(
            NAME,
            format!(
                "{} does not exist yet; it is made on first use",
                home.display()
            ),
        ),
        Err(e) => fail(NAME, format!("{}: {e}", home.display())),
    }
}

fn connections(home: &Path, config: &Config) -> Vec<Check> {
    if config.connections.is_empty() {
        return vec![fail(
            "connections",
            "none is set up; run `tiphys` and add one in the app",
        )];
    }
    let mut checks: Vec<Check> = config
        .connections
        .iter()
        .map(|(name, connection)| {
            let title = format!("connection `{name}`");
            let Some(model) = &connection.model else {
                return fail(&title, "no model chosen; pick one in the app with /model");
            };
            match keys::resolve(home, name, connection.env_key.as_deref()) {
                Ok(Some(_)) => pass(&title, format!("{model}, with a key")),
                Ok(None) if connection.local => {
                    pass(&title, format!("{model}, local, no key needed"))
                }
                Ok(None) => fail(
                    &title,
                    format!(
                        "{model}, but no key is stored; enter one in the app with /connections"
                    ),
                ),
                Err(e) => fail(&title, e.to_string()),
            }
        })
        .collect();
    if config.starting_connection().is_none() {
        checks.push(fail(
            "default connection",
            "there are several connections and none is the default; choose one in the app",
        ));
    }
    checks
}

/// Whether Telegram is set up. Not being set up is not a failure.
fn telegram(home: &Path, config: &Config) -> Check {
    const NAME: &str = "telegram";
    let users = config.telegram.allow.len();
    match (keys::is_stored(home, keys::TELEGRAM), users) {
        (false, _) => pass(NAME, "not set up; type /telegram in the app to add a bot"),
        (true, 0) => pass(
            NAME,
            "a bot is set up and answers nobody yet; type /telegram in the app to pair",
        ),
        (true, 1) => pass(NAME, "a bot is set up and answers 1 user"),
        (true, users) => pass(NAME, format!("a bot is set up and answers {users} users")),
    }
}

fn key_store(home: &Path) -> Check {
    const NAME: &str = "key store";
    let dir = home.join(KEYS_DIR);
    let mode = |path: &Path| std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777);
    let dir_mode = match mode(&dir) {
        Ok(dir_mode) => dir_mode,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return pass(NAME, "no keys are stored");
        }
        Err(e) => return fail(NAME, format!("{}: {e}", dir.display())),
    };
    if dir_mode & 0o077 != 0 {
        return fail(
            NAME,
            format!(
                "{} can be entered by other users; run chmod 700 on it",
                dir.display()
            ),
        );
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return fail(NAME, format!("{} cannot be listed", dir.display()));
    };
    let mut count = 0;
    for entry in entries.flatten() {
        count += 1;
        if mode(&entry.path()).is_ok_and(|file_mode| file_mode & 0o077 != 0) {
            return fail(
                NAME,
                format!(
                    "{} can be read by other users; run chmod 600 on it",
                    entry.path().display()
                ),
            );
        }
    }
    pass(NAME, format!("{count} keys, readable only by this user"))
}

fn action_log(home: &Path) -> Check {
    const NAME: &str = "action log";
    match ActionLog::at(home).verify() {
        Ok(0) => pass(NAME, "empty"),
        Ok(count) => pass(
            NAME,
            format!("{count} entries, each following from the one before"),
        ),
        Err(e) => fail(NAME, e.to_string()),
    }
}

fn bash() -> Check {
    const NAME: &str = "bash";
    match std::process::Command::new("bash")
        .args(["-c", "true"])
        .status()
    {
        Ok(status) if status.success() => pass(NAME, "found; the shell tool can run commands"),
        Ok(status) => fail(NAME, format!("it is there but does not run: {status}")),
        Err(e) => fail(
            NAME,
            format!("not found, so the shell tool cannot run commands: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actionlog::{Gate, Record};
    use crate::config::Connection;
    use crate::keys::Secret;
    use crate::llm::{Delta, Provider, ReplayProvider, ToolCallPart};
    use crate::settings;
    use std::sync::Arc;

    fn connection(model: Option<&str>, local: bool) -> Connection {
        Connection {
            base_url: "https://api.example.com/v1".into(),
            model: model.map(Into::into),
            env_key: None,
            local,
        }
    }

    /// The checks by name, leaving out the one about what is installed here.
    fn found(home: &Path) -> Vec<(String, bool)> {
        run(home)
            .into_iter()
            .filter(|c| c.name != "bash")
            .map(|c| (c.name, c.ok))
            .collect()
    }

    fn detail(home: &Path, name: &str) -> String {
        run(home)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap()
            .detail
    }

    #[test]
    fn a_first_run_is_told_to_set_up_a_connection() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state");
        let checks = found(&home);
        assert_eq!(
            checks,
            [
                ("state directory".to_string(), true),
                ("configuration".to_string(), true),
                ("connections".to_string(), false),
                ("telegram".to_string(), true),
                ("key store".to_string(), true),
                ("action log".to_string(), true),
            ]
        );
        assert!(detail(&home, "connections").contains("add one in the app"));
        // Looking changed nothing.
        assert!(!home.exists());
    }

    #[test]
    fn each_connection_is_checked_for_a_model_and_a_key() {
        let home = tempfile::tempdir().unwrap();
        settings::save_connection(home.path(), "good", &connection(Some("m"), false)).unwrap();
        keys::store(home.path(), "good", &Secret::new("sk-1").unwrap()).unwrap();
        settings::save_connection(home.path(), "keyless", &connection(Some("m"), false)).unwrap();
        settings::save_connection(home.path(), "local", &connection(Some("m"), true)).unwrap();
        settings::save_connection(home.path(), "unchosen", &connection(None, true)).unwrap();

        let checks = found(home.path());
        let of = |name: &str| checks.iter().find(|(n, _)| n == name).unwrap().1;
        assert!(of("connection `good`"));
        assert!(!of("connection `keyless`"));
        assert!(of("connection `local`"));
        assert!(!of("connection `unchosen`"));
        assert!(!of("default connection"));
        assert!(of("key store"));
        // No check shows a key.
        assert!(!format!("{:?}", run(home.path())).contains("sk-1"));
    }

    #[test]
    fn keys_others_can_read_and_a_broken_log_are_found() {
        let home = tempfile::tempdir().unwrap();
        keys::store(home.path(), "work", &Secret::new("sk-1").unwrap()).unwrap();
        let key = home.path().join("keys/work");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(detail(home.path(), "key store").contains("chmod 600"));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(
            home.path().join("keys"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(detail(home.path(), "key store").contains("chmod 700"));

        let log = ActionLog::at(home.path());
        for _ in 0..2 {
            log.append(Record {
                session: "s".into(),
                audience: "terminal".into(),
                tool: "read_file".into(),
                class: None,
                summary: "read x".into(),
                reason: String::new(),
                gate: Gate::Free,
                ok: true,
            })
            .unwrap();
        }
        assert_eq!(
            detail(home.path(), "action log"),
            "2 entries, each following from the one before"
        );
        let file = std::fs::read_dir(home.path().join("log"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .unwrap();
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("read x", "read y");
        std::fs::write(&file, text).unwrap();
        assert!(detail(home.path(), "action log").contains("changed after it was written"));

        std::fs::write(home.path().join("config.toml"), "not toml =").unwrap();
        assert!(
            !found(home.path())
                .iter()
                .find(|(n, _)| n == "configuration")
                .unwrap()
                .1
        );
    }

    struct Scripted(Arc<dyn Provider>);

    impl Connect for Scripted {
        fn provider(&self, _: &Connection, _: Option<&Secret>) -> crate::Result<Arc<dyn Provider>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn the_live_check_makes_a_tool_call_on_the_starting_connection() {
        let home = tempfile::tempdir().unwrap();
        let none = Scripted(Arc::new(ReplayProvider::default()));
        assert!(!live(home.path(), &none).await.ok);

        settings::save_connection(home.path(), "work", &connection(Some("vendor/model"), true))
            .unwrap();
        let pings = vec![
            Delta::ToolCall(ToolCallPart {
                slot: Some(0),
                id: "c".into(),
                name: "ping".into(),
                arguments: r#"{"word":"anchor"}"#.into(),
            }),
            Delta::Done,
        ];
        let working = Scripted(Arc::new(ReplayProvider::new(vec![
            pings,
            vec![Delta::Text("pong".into()), Delta::Done],
        ])));
        let check = live(home.path(), &working).await;
        assert!(check.ok, "{}", check.detail);
        assert_eq!(
            check.detail,
            "vendor/model on `work` called a tool and read its result"
        );

        let silent = Scripted(Arc::new(ReplayProvider::new(vec![vec![
            Delta::Text("no tools here".into()),
            Delta::Done,
        ]])));
        let check = live(home.path(), &silent).await;
        assert!(
            !check.ok && check.detail.contains("instead of calling the tool"),
            "{}",
            check.detail
        );
    }
}
