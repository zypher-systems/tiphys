//! Where Tiphys keeps its state, and what has been configured.
//!
//! State lives in one directory: `~/.tiphys`, or wherever `TIPHYS_HOME`
//! points. Two files in it hold configuration:
//!
//! ```toml
//! # config.toml: the owner's file. Tiphys never rewrites it.
//! default_connection = "openrouter"
//!
//! [connections.openrouter]
//! base_url = "https://openrouter.ai/api/v1"
//! ```
//!
//! ```toml
//! # settings.toml: what the app saved.
//! [connections.openrouter]
//! model = "vendor/model-id"
//! ```
//!
//! The two are merged key by key, and where both set the same key
//! `settings.toml` wins: it holds the choice made most recently, in the app.
//! Keys are in neither file; see [`crate::keys`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::spend::Rates;
use crate::{Error, Result};

/// The variable that moves the state directory.
pub const HOME_ENV: &str = "TIPHYS_HOME";
/// The owner's file.
pub const CONFIG_FILE: &str = "config.toml";
/// The app's file.
pub const SETTINGS_FILE: &str = "settings.toml";

/// The state directory for this process.
pub fn home_dir() -> Result<PathBuf> {
    home_dir_with(|name| std::env::var_os(name), dirs::home_dir())
}

/// [`home_dir`] with the environment and the user's home passed in.
pub fn home_dir_with(
    env: impl Fn(&str) -> Option<OsString>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(value) = env(HOME_ENV).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(value);
        // A relative path would mean a different directory for every place
        // Tiphys is started from.
        if !path.is_absolute() {
            return Err(Error::Config(format!(
                "{HOME_ENV} must be an absolute path, got {}",
                path.display()
            )));
        }
        return Ok(path);
    }
    user_home
        .map(|home| home.join(".tiphys"))
        .ok_or_else(|| Error::Config(format!("no home directory found; set {HOME_ENV}")))
}

/// The home of the user Tiphys runs as. Relative paths the model gives start
/// here.
pub fn user_home() -> Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| Error::Config("the user Tiphys runs as has no home directory".into()))
}

/// Everything configured, after `settings.toml` is laid over `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The connection a new session uses.
    pub default_connection: Option<String>,
    /// Model endpoints, by name.
    pub connections: BTreeMap<String, Connection>,
    /// The owner's prices by model id, in dollars per million tokens. They
    /// win over what a provider lists, and price a model that has no listing.
    pub pricing: BTreeMap<String, Rates>,
    /// The bounds on one turn.
    pub limits: Limits,
    /// What asks before it runs.
    pub approvals: Approvals,
    /// Who may reach the daemon.
    pub daemon: Daemon,
}

/// Who may reach the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Daemon {
    /// The numeric ids of the users who own this agent, besides the user the
    /// daemon runs as and root. Anyone else who reaches the socket is turned
    /// away.
    pub owners: Vec<u32>,
    /// The command that starts the worker which acts for the agent as
    /// another user: a program and its arguments. Empty means there is no
    /// worker, and the agent acts as the user Tiphys itself runs as.
    pub worker: Vec<String>,
}

/// What asks before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Approvals {
    /// Changes inside the agent's own home. Anything beyond it always asks.
    pub change: OnChange,
}

/// What a change inside the agent's own home does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnChange {
    /// It runs. The machine exists for the agent.
    #[default]
    Run,
    /// It asks first, like everything else that changes something.
    Ask,
}

/// The bounds on one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Model calls in one turn. A turn that reaches this stops and says so.
    pub rounds: u32,
    /// The most a reply may be, in tokens. Unset leaves it to the provider.
    pub max_tokens: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            rounds: 40,
            max_tokens: None,
        }
    }
}

/// One model endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    /// The endpoint's root, such as `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// The model a new session uses on this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// An environment variable to read the key from, for a server that starts
    /// with nobody present. The stored key is used when this is unset or empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// The server runs on the owner's own hardware: it costs nothing, and it
    /// is given longer to answer, since it may have to load a model first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

impl Config {
    /// The connection a new session uses and its name: the default if one is
    /// set, otherwise the only connection there is.
    pub fn starting_connection(&self) -> Option<(&str, &Connection)> {
        let name = match &self.default_connection {
            Some(name) => name.as_str(),
            None if self.connections.len() == 1 => self.connections.keys().next()?.as_str(),
            None => return None,
        };
        self.connections
            .get_key_value(name)
            .map(|(name, connection)| (name.as_str(), connection))
    }

    fn validate(&self) -> Result<()> {
        for (name, connection) in &self.connections {
            valid_name(name)?;
            connection
                .validate()
                .map_err(|e| Error::Config(format!("connection `{name}`: {e}")))?;
        }
        for (model, rates) in &self.pricing {
            if !rates.is_usable() {
                return Err(Error::Config(format!(
                    "pricing for `{model}`: a rate must be a number that is not negative"
                )));
            }
        }
        if self.limits.rounds == 0 || self.limits.max_tokens == Some(0) {
            return Err(Error::Config(
                "limits: rounds and max_tokens must be at least 1".into(),
            ));
        }
        if let Some(name) = &self.default_connection
            && !self.connections.contains_key(name)
        {
            return Err(Error::Config(format!(
                "default_connection names `{name}`, which is not a connection"
            )));
        }
        Ok(())
    }
}

impl Connection {
    /// The base URL without a trailing slash, ready to have a path added.
    pub fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Checks one connection. The message says what is wrong, not which
    /// connection; the caller knows the name.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let rest = self
            .base_url
            .strip_prefix("https://")
            .or_else(|| self.base_url.strip_prefix("http://"))
            .ok_or("base_url must start with http:// or https://")?;
        if rest.trim_matches('/').is_empty() || rest.contains(char::is_whitespace) {
            return Err(format!("base_url `{}` is not a URL", self.base_url));
        }
        if let Some(model) = &self.model
            && model.trim().is_empty()
        {
            return Err("model is empty".into());
        }
        if let Some(env_key) = &self.env_key
            && !is_env_name(env_key)
        {
            return Err(format!("env_key `{env_key}` is not a variable name"));
        }
        Ok(())
    }
}

/// Checks a name that becomes a file name and a TOML key: a connection, and
/// later a job or a skill. Lowercase letters, digits, `-` and `_`; it starts
/// with a letter or digit; at most 64 characters.
pub fn valid_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if first_ok && rest_ok && name.len() <= 64 {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "`{name}` is not a usable name: use lowercase letters, digits, - and _, \
             starting with a letter or digit, up to 64 characters"
        )))
    }
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Loads the configuration in `home`. Missing files are an empty
/// configuration, which is how a first run starts.
pub fn load_at(home: &Path) -> Result<Config> {
    let mut merged = read_table(&home.join(CONFIG_FILE))?;
    merge(&mut merged, read_table(&home.join(SETTINGS_FILE))?);
    let config: Config = merged
        .try_into()
        .map_err(|e| Error::Config(format!("{CONFIG_FILE} and {SETTINGS_FILE}: {e}")))?;
    config.validate()?;
    Ok(config)
}

fn read_table(path: &Path) -> Result<toml::Table> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(toml::Table::new()),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    text.parse()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

/// Lays `over` on `base`. Tables merge key by key; anything else is replaced.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(below)), toml::Value::Table(above)) => merge(below, above),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(config: &str, settings: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        if !config.is_empty() {
            std::fs::write(home.path().join(CONFIG_FILE), config).unwrap();
        }
        if !settings.is_empty() {
            std::fs::write(home.path().join(SETTINGS_FILE), settings).unwrap();
        }
        home
    }

    fn connection(base_url: &str) -> Connection {
        Connection {
            base_url: base_url.into(),
            model: None,
            env_key: None,
            local: false,
        }
    }

    #[test]
    fn the_home_comes_from_the_variable_and_falls_back_to_the_users_home() {
        let user_home = Some(PathBuf::from("/home/ada"));
        let set = |value: &'static str| move |_: &str| Some(OsString::from(value));

        assert_eq!(
            home_dir_with(set("/srv/tiphys"), user_home.clone()).unwrap(),
            PathBuf::from("/srv/tiphys")
        );
        assert_eq!(
            home_dir_with(|_| None, user_home.clone()).unwrap(),
            PathBuf::from("/home/ada/.tiphys")
        );
        // An empty variable is the same as an unset one.
        assert_eq!(
            home_dir_with(set(""), user_home.clone()).unwrap(),
            PathBuf::from("/home/ada/.tiphys")
        );
        assert!(home_dir_with(set("state"), user_home).is_err());
        assert!(home_dir_with(|_| None, None).is_err());
    }

    #[test]
    fn a_first_run_has_no_files_and_an_empty_config() {
        let home = tempfile::tempdir().unwrap();
        let config = load_at(home.path()).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.starting_connection().is_none());
    }

    #[test]
    fn settings_are_laid_over_config_key_by_key() {
        let home = home_with(
            r#"
            default_connection = "work"
            [connections.work]
            base_url = "https://api.example.com/v1/"
            model = "old"
            env_key = "WORK_KEY"
            [connections.local]
            base_url = "http://127.0.0.1:11434/v1"
            "#,
            r#"
            default_connection = "local"
            [connections.work]
            model = "new"
            "#,
        );
        let config = load_at(home.path()).unwrap();
        let work = &config.connections["work"];
        assert_eq!(work.model.as_deref(), Some("new"));
        assert_eq!(work.env_key.as_deref(), Some("WORK_KEY"));
        assert_eq!(work.base(), "https://api.example.com/v1");
        assert_eq!(config.starting_connection().unwrap().0, "local");
    }

    #[test]
    fn a_local_connection_and_the_owners_prices_are_read() {
        let home = home_with(
            r#"
            [connections.local]
            base_url = "http://127.0.0.1:11434/v1"
            local = true

            [pricing."vendor/model"]
            input = 3.0
            output = 15.0
            cache_read = 0.3
            "#,
            "",
        );
        let config = load_at(home.path()).unwrap();
        assert!(config.connections["local"].local);
        let rates = config.pricing["vendor/model"];
        assert_eq!((rates.input, rates.output), (3.0, 15.0));
        assert_eq!((rates.cache_read, rates.cache_write), (Some(0.3), None));
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.approvals.change, OnChange::Run);
    }

    #[test]
    fn changes_can_be_made_to_ask() {
        let home = home_with(
            "[approvals]\nchange = \"ask\"\n\n[daemon]\nowners = [1000, 1001]\n",
            "",
        );
        let config = load_at(home.path()).unwrap();
        assert_eq!(config.approvals.change, OnChange::Ask);
        assert_eq!(config.daemon.owners, [1000, 1001]);
    }

    #[test]
    fn the_only_connection_is_the_default_and_two_need_a_choice() {
        let mut config = Config::default();
        config
            .connections
            .insert("a".into(), connection("https://a.example/v1"));
        assert_eq!(config.starting_connection().unwrap().0, "a");

        config
            .connections
            .insert("b".into(), connection("https://b.example/v1"));
        assert!(config.starting_connection().is_none());

        config.default_connection = Some("b".into());
        assert_eq!(config.starting_connection().unwrap().0, "b");
    }

    #[test]
    fn a_config_tiphys_cannot_use_is_refused_with_the_reason() {
        let cases = [
            ("default_connection = \"gone\"", "not a connection"),
            (
                "[connections.Work]\nbase_url = \"https://a.example\"",
                "not a usable name",
            ),
            (
                "[connections.a]\nbase_url = \"a.example/v1\"",
                "must start with http",
            ),
            ("[connections.a]\nbase_url = \"https://\"", "is not a URL"),
            (
                "[connections.a]\nbase_url = \"https://a.example\"\nmodel = \" \"",
                "model is empty",
            ),
            (
                "[connections.a]\nbase_url = \"https://a.example\"\nenv_key = \"MY-KEY\"",
                "not a variable name",
            ),
            (
                "[connections.a]\nbase_url = \"https://a.example\"\napi_key = \"sk-1\"",
                "unknown field",
            ),
            ("[connections.a]\nmodel = \"m\"", "base_url"),
            (
                "[pricing.\"vendor/model\"]\ninput = -1.0\noutput = 2.0",
                "not negative",
            ),
            ("[pricing.m]\ninput = 1.0", "output"),
            ("[limits]\nrounds = 0", "at least 1"),
            ("[limits]\nturns = 3", "unknown field"),
            ("[approvals]\nchange = \"never\"", "unknown variant"),
            ("default_connection = ", "config.toml"),
        ];
        for (text, expected) in cases {
            let home = home_with(text, "");
            let err = load_at(home.path()).unwrap_err().to_string();
            assert!(err.contains(expected), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn names_that_become_file_names_are_checked() {
        for name in ["a", "openrouter", "work-2", "x_y", "9lives"] {
            assert!(valid_name(name).is_ok(), "{name}");
        }
        let long = "a".repeat(65);
        for name in [
            "", "-a", "_a", "Work", "a b", "a/b", "..", "a.b", "é", &long,
        ] {
            assert!(valid_name(name).is_err(), "{name}");
        }
    }
}
