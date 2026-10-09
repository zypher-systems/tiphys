//! What the app saves: `settings.toml`.
//!
//! The owner's `config.toml` is never rewritten, so every choice made in the
//! app lands here instead and is laid over it when the configuration is
//! loaded (see [`crate::config`]). The file is edited in place: a key the app
//! does not touch, and any comment the owner added, stay as they were.
//!
//! ```toml
//! default_connection = "openrouter"
//!
//! [connections.openrouter]
//! base_url = "https://openrouter.ai/api/v1"
//! model = "vendor/model-id"
//! ```

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{Connection, SETTINGS_FILE, valid_name};
use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir, write_atomic};
use crate::{Error, Result};

/// Saves a connection, replacing one of the same name.
pub fn save_connection(home: &Path, name: &str, connection: &Connection) -> Result<()> {
    valid_name(name)?;
    connection
        .validate()
        .map_err(|e| Error::Config(format!("connection `{name}`: {e}")))?;

    let mut table = Table::new();
    table["base_url"] = value(connection.base());
    if let Some(model) = &connection.model {
        table["model"] = value(model);
    }
    if let Some(env_key) = &connection.env_key {
        table["env_key"] = value(env_key);
    }
    if connection.local {
        table["local"] = value(true);
    }
    edit(home, |doc| {
        connections(doc)?.insert(name, Item::Table(table));
        Ok(())
    })
}

/// Sets the model a connection starts sessions with. The connection itself
/// may be defined in `config.toml`; only the model is written here.
pub fn set_model(home: &Path, connection: &str, model: &str) -> Result<()> {
    valid_name(connection)?;
    if model.trim().is_empty() {
        return Err(Error::Config("model is empty".into()));
    }
    edit(home, |doc| {
        let entry = connections(doc)?
            .entry(connection)
            .or_insert(Item::Table(Table::new()));
        let table = entry
            .as_table_mut()
            .ok_or_else(|| not_a_table(&format!("connections.{connection}")))?;
        table["model"] = value(model.trim());
        Ok(())
    })
}

/// Chooses the connection a new session uses, or clears the choice.
pub fn set_default_connection(home: &Path, name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        valid_name(name)?;
    }
    edit(home, |doc| {
        match name {
            Some(name) => doc["default_connection"] = value(name),
            None => {
                doc.remove("default_connection");
            }
        }
        Ok(())
    })
}

/// Removes a connection the app saved, and the default if it pointed there.
/// Returns whether there was one. A connection defined in `config.toml` is
/// the owner's to remove.
pub fn remove_connection(home: &Path, name: &str) -> Result<bool> {
    let mut removed = false;
    edit(home, |doc| {
        removed = connections(doc)?.remove(name).is_some();
        let was_default = doc.get("default_connection").and_then(Item::as_str) == Some(name);
        if removed && was_default {
            doc.remove("default_connection");
        }
        Ok(())
    })?;
    Ok(removed)
}

/// Sets what an install decides: `owner` is added to those who may talk to
/// the daemon, and `worker` is the command that starts its worker.
pub fn set_daemon(home: &Path, owner: u32, worker: &[String]) -> Result<()> {
    edit(home, |doc| {
        let daemon = doc
            .entry("daemon")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| not_a_table("daemon"))?;
        let owners = daemon
            .entry("owners")
            .or_insert(value(toml_edit::Array::new()))
            .as_array_mut()
            .ok_or_else(|| {
                Error::Config(format!("{SETTINGS_FILE}: `daemon.owners` is not a list"))
            })?;
        let listed = owners
            .iter()
            .any(|listed| listed.as_integer() == Some(i64::from(owner)));
        if !listed {
            owners.push(i64::from(owner));
        }
        daemon["worker"] = value(
            worker
                .iter()
                .map(String::as_str)
                .collect::<toml_edit::Array>(),
        );
        Ok(())
    })
}

/// Adds a Telegram user to those the agent answers.
pub fn allow_telegram(home: &Path, user: i64) -> Result<()> {
    // A list here stands in for the one in the owner's file, so it starts
    // from everyone who is allowed now.
    let already = crate::config::load_at(home)?.telegram.allow;
    edit(home, |doc| {
        let telegram = doc
            .entry("telegram")
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| not_a_table("telegram"))?;
        let allow = telegram
            .entry("allow")
            .or_insert(value(toml_edit::Array::new()))
            .as_array_mut()
            .ok_or_else(|| {
                Error::Config(format!("{SETTINGS_FILE}: `telegram.allow` is not a list"))
            })?;
        for user in already.into_iter().chain([user]) {
            if !allow.iter().any(|listed| listed.as_integer() == Some(user)) {
                allow.push(user);
            }
        }
        Ok(())
    })
}

fn edit(home: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let path = home.join(SETTINGS_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    change(&mut doc)?;
    ensure_dir(home, SHARED_DIR)?;
    space_sections(&mut doc)?;
    // The first section needs no blank line above it.
    let text = doc.to_string();
    write_atomic(&path, text.trim_start_matches('\n').as_bytes(), SHARED_FILE)
}

fn connections(doc: &mut DocumentMut) -> Result<&mut Table> {
    let table = doc
        .entry("connections")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| not_a_table("connections"))?;
    // No bare `[connections]` header above the first `[connections.<name>]`.
    table.set_implicit(true);
    Ok(table)
}

/// Puts a blank line above every `[connections.<name>]` header. A table built
/// here has none, and the file is one a person reads.
fn space_sections(doc: &mut DocumentMut) -> Result<()> {
    for (_, item) in connections(doc)?.iter_mut() {
        let Some(table) = item.as_table_mut() else {
            continue;
        };
        let above = table
            .decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or("");
        if !above.starts_with('\n') {
            let spaced = format!("\n{above}");
            table.decor_mut().set_prefix(spaced);
        }
    }
    Ok(())
}

fn not_a_table(key: &str) -> Error {
    Error::Config(format!("{SETTINGS_FILE}: `{key}` is not a table"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CONFIG_FILE, load_at};

    fn connection(base_url: &str, model: Option<&str>) -> Connection {
        Connection {
            base_url: base_url.into(),
            model: model.map(Into::into),
            env_key: None,
            local: false,
        }
    }

    fn settings_text(home: &Path) -> String {
        std::fs::read_to_string(home.join(SETTINGS_FILE)).unwrap()
    }

    #[test]
    fn a_saved_connection_is_there_on_the_next_load() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        save_connection(
            &state,
            "openrouter",
            &connection("https://openrouter.ai/api/v1/", Some("m")),
        )
        .unwrap();
        set_default_connection(&state, Some("openrouter")).unwrap();

        let config = load_at(&state).unwrap();
        let (name, saved) = config.starting_connection().unwrap();
        assert_eq!(name, "openrouter");
        assert_eq!(saved.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(saved.model.as_deref(), Some("m"));
        assert_eq!(
            settings_text(&state),
            "default_connection = \"openrouter\"\n\n[connections.openrouter]\n\
             base_url = \"https://openrouter.ai/api/v1\"\nmodel = \"m\"\n"
        );
    }

    #[test]
    fn a_model_choice_is_laid_over_a_connection_from_config() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(CONFIG_FILE),
            "[connections.work]\nbase_url = \"https://api.example.com/v1\"\nmodel = \"old\"\n",
        )
        .unwrap();
        set_model(home.path(), "work", " new ").unwrap();

        let config = load_at(home.path()).unwrap();
        assert_eq!(config.connections["work"].model.as_deref(), Some("new"));
        // The owner's file is as it was.
        let owners = std::fs::read_to_string(home.path().join(CONFIG_FILE)).unwrap();
        assert!(owners.contains("model = \"old\""));
    }

    #[test]
    fn an_edit_keeps_what_it_does_not_touch() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(SETTINGS_FILE),
            "# mine\n[connections.a]\nbase_url = \"https://a.example/v1\" # keep\n",
        )
        .unwrap();
        save_connection(home.path(), "b", &connection("https://b.example/v1", None)).unwrap();
        set_model(home.path(), "a", "m").unwrap();

        let text = settings_text(home.path());
        assert!(text.contains("# mine"), "{text}");
        assert!(text.contains("# keep"), "{text}");
        let config = load_at(home.path()).unwrap();
        assert_eq!(config.connections.len(), 2);
        assert_eq!(config.connections["a"].model.as_deref(), Some("m"));
    }

    #[test]
    fn removing_a_connection_clears_a_default_that_pointed_at_it() {
        let home = tempfile::tempdir().unwrap();
        save_connection(home.path(), "a", &connection("https://a.example/v1", None)).unwrap();
        save_connection(home.path(), "b", &connection("https://b.example/v1", None)).unwrap();
        set_default_connection(home.path(), Some("a")).unwrap();

        assert!(remove_connection(home.path(), "a").unwrap());
        assert!(!remove_connection(home.path(), "a").unwrap());
        let config = load_at(home.path()).unwrap();
        assert_eq!(config.default_connection, None);
        assert_eq!(config.starting_connection().unwrap().0, "b");
    }

    #[test]
    fn an_install_adds_its_owner_and_sets_the_worker_without_losing_the_rest() {
        let home = tempfile::tempdir().unwrap();
        save_connection(
            home.path(),
            "a",
            &connection("https://a.example/v1", Some("m")),
        )
        .unwrap();
        let worker = |binary: &str| {
            vec![
                "sudo".to_string(),
                "-u".into(),
                "tiphys".into(),
                binary.into(),
                "worker".into(),
            ]
        };
        set_daemon(home.path(), 1000, &worker("/usr/local/bin/tiphys")).unwrap();
        // A second install: another owner, and the binary somewhere else.
        set_daemon(home.path(), 1001, &worker("/usr/bin/tiphys")).unwrap();
        set_daemon(home.path(), 1000, &worker("/usr/bin/tiphys")).unwrap();

        let config = load_at(home.path()).unwrap();
        assert_eq!(config.daemon.owners, [1000, 1001]);
        assert_eq!(config.daemon.worker, worker("/usr/bin/tiphys"));
        assert_eq!(config.connections["a"].model.as_deref(), Some("m"));
    }

    #[test]
    fn a_telegram_user_is_allowed_once_however_often_it_is_asked() {
        let home = tempfile::tempdir().unwrap();
        allow_telegram(home.path(), 42).unwrap();
        allow_telegram(home.path(), 7).unwrap();
        allow_telegram(home.path(), 42).unwrap();
        assert_eq!(load_at(home.path()).unwrap().telegram.allow, [42, 7]);
    }

    #[test]
    fn allowing_a_telegram_user_keeps_those_the_owner_listed_by_hand() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[telegram]\nallow = [1, 2]\n",
        )
        .unwrap();
        allow_telegram(home.path(), 3).unwrap();
        assert_eq!(load_at(home.path()).unwrap().telegram.allow, [1, 2, 3]);
    }

    #[test]
    fn nothing_is_written_when_the_input_is_refused() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            save_connection(
                home.path(),
                "Bad Name",
                &connection("https://a.example", None)
            )
            .is_err()
        );
        assert!(save_connection(home.path(), "a", &connection("ftp://a.example", None)).is_err());
        assert!(set_model(home.path(), "a", "  ").is_err());
        assert!(set_default_connection(home.path(), Some("../x")).is_err());
        assert!(!home.path().join(SETTINGS_FILE).exists());
    }
}
