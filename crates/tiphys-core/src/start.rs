//! Putting an agent together from what is configured.
//!
//! Every way of running a turn starts here: the terminal app, a one-shot
//! run, and later the daemon. It picks the connection and model, finds the
//! key, loads the prices, and opens a session or begins a new one.

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;

use crate::actionlog::ActionLog;
use crate::agent::Agent;
use crate::approval::{Approver, DenyAll};
use crate::cancel::Cancel;
use crate::config::{self, Config, Connection, OnChange};
use crate::llm::{ChatConnect, Connect, Provider, catalog};
use crate::prompt::{Machine, system_prompt};
use crate::session::{self, Opening, Session};
use crate::spend::PriceBook;
use crate::tools::{Registry, ToolCtx};
use crate::{Error, Result, keys};

/// Which session a run uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Resume {
    /// A new one.
    #[default]
    New,
    /// The one most recently begun, or a new one if there is none.
    Latest,
    /// This one.
    Id(String),
}

/// What the caller chose. Anything left out comes from the configuration.
#[derive(Debug, Clone, Default)]
pub struct Start {
    pub connection: Option<String>,
    pub model: Option<String>,
    pub resume: Resume,
    /// Who the session is with.
    pub audience: String,
}

/// Builds the agent for a run with nobody to ask: what needs a yes does not
/// run.
pub async fn agent(home: &Path, start: Start) -> Result<Agent> {
    let user_home = config::user_home()?;
    agent_for(home, &user_home, start, &ChatConnect, Arc::new(DenyAll)).await
}

/// [`agent`] with the home of the user Tiphys runs as passed in.
pub async fn agent_for(
    home: &Path,
    user_home: &Path,
    start: Start,
    connect: &dyn Connect,
    approver: Arc<dyn Approver>,
) -> Result<Agent> {
    let config = config::load_at(home)?;
    let ctx = ToolCtx {
        state: home.to_path_buf(),
        cwd: user_home.to_path_buf(),
        home: user_home.to_path_buf(),
    };
    let tools = Registry::builtin();

    let resumed = match &start.resume {
        Resume::New => None,
        Resume::Id(id) => Some(Session::open(home, id)?),
        Resume::Latest => match session::list(home)?.first() {
            Some(latest) => Some(Session::open(home, &latest.id)?),
            None => None,
        },
    };
    let session = match resumed {
        // A session keeps the connection and model it began with.
        Some(session) => session,
        None => {
            let (name, connection) = pick_connection(&config, start.connection.as_deref())?;
            let model = start
                .model
                .clone()
                .or_else(|| connection.model.clone())
                .ok_or_else(|| {
                    Error::Config(format!(
                        "connection `{name}` has no model chosen; run `tiphys` and pick one in the app"
                    ))
                })?;
            let machine = Machine::detect(&ctx.home);
            Session::create(
                home,
                Opening {
                    connection: name.to_string(),
                    model,
                    audience: start.audience.clone(),
                    system: system_prompt(&machine, Utc::now()),
                    tools: tools.specs(),
                },
            )?
        }
    };

    let name = session.meta().connection.clone();
    let connection = config.connections.get(&name).ok_or_else(|| {
        Error::Config(format!(
            "this session uses the connection `{name}`, which is no longer set up"
        ))
    })?;
    let key = keys::resolve(home, &name, connection.env_key.as_deref())?;
    let provider = connect.provider(connection, key.as_ref())?;
    let prices = prices(home, &config, &name, provider.as_ref()).await;

    Ok(Agent {
        provider,
        session,
        tools,
        ctx,
        prices,
        local: connection.local,
        limits: config.limits,
        cancel: Arc::new(Cancel::default()),
        approver,
        ask_before_change: config.approvals.change == OnChange::Ask,
        actions: ActionLog::at(home),
    })
}

fn pick_connection<'a>(
    config: &'a Config,
    asked: Option<&str>,
) -> Result<(&'a str, &'a Connection)> {
    match asked {
        Some(asked) => config
            .connections
            .get_key_value(asked)
            .map(|(name, connection)| (name.as_str(), connection))
            .ok_or_else(|| Error::Config(format!("there is no connection named `{asked}`"))),
        None => config.starting_connection().ok_or_else(|| {
            Error::Config(if config.connections.is_empty() {
                "no connection is set up yet; run `tiphys` and add one in the app".into()
            } else {
                "there are several connections and no default; name one with --connection, or \
                 choose the default in the app"
                    .into()
            })
        }),
    }
}

/// The prices for a run: the owner's, and the connection's model list. If the
/// list was never kept it is fetched now; a failure to fetch it only means
/// costs show as unknown.
async fn prices(home: &Path, config: &Config, name: &str, provider: &dyn Provider) -> PriceBook {
    let mut book = PriceBook::new(config.pricing.clone());
    let models = match catalog::load(home, name) {
        Some(kept) => kept.models,
        None => match provider.models().await {
            Ok(models) => {
                let _ = catalog::store(home, name, &models);
                models
            }
            Err(_) => Vec::new(),
        },
    };
    book.learn(name, &models);
    book
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    fn connection(model: Option<&str>) -> Connection {
        Connection {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: model.map(Into::into),
            env_key: None,
            local: true,
        }
    }

    /// Saves a connection with an empty model list already kept, so building
    /// an agent has nothing to fetch and never opens a socket.
    fn save(home: &Path, name: &str, connection: &Connection) {
        settings::save_connection(home, name, connection).unwrap();
        catalog::store(home, name, &[]).unwrap();
    }

    async fn agent(home: &Path, start: Start) -> Result<Agent> {
        agent_for(
            home,
            &home.join("user"),
            start,
            &ChatConnect,
            Arc::new(DenyAll),
        )
        .await
    }

    fn start() -> Start {
        Start {
            audience: "terminal".into(),
            ..Start::default()
        }
    }

    async fn refusal(home: &Path, start: Start) -> String {
        match agent(home, start).await {
            Ok(_) => panic!("an agent was built"),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn without_a_connection_it_says_where_to_set_one_up() {
        let home = tempfile::tempdir().unwrap();
        let message = refusal(home.path(), start()).await;
        assert!(message.contains("add one in the app"), "{message}");
    }

    #[tokio::test]
    async fn what_is_missing_or_ambiguous_is_named() {
        let home = tempfile::tempdir().unwrap();
        save(home.path(), "a", &connection(None));
        let message = refusal(home.path(), start()).await;
        assert!(
            message.contains("connection `a` has no model chosen"),
            "{message}"
        );

        save(home.path(), "b", &connection(Some("m")));
        let message = refusal(home.path(), start()).await;
        assert!(
            message.contains("several connections and no default"),
            "{message}"
        );

        let asked = Start {
            connection: Some("zzz".into()),
            ..start()
        };
        let message = refusal(home.path(), asked).await;
        assert!(message.contains("no connection named `zzz`"), "{message}");

        let gone = Start {
            resume: Resume::Id("0199aaaa-0000-7000-8000-000000000000".into()),
            ..start()
        };
        let message = refusal(home.path(), gone).await;
        assert!(message.contains("no session"), "{message}");
    }

    #[tokio::test]
    async fn a_new_session_gets_its_connection_model_prompt_and_tools() {
        let home = tempfile::tempdir().unwrap();
        save(home.path(), "a", &connection(Some("from-settings")));
        save(home.path(), "b", &connection(Some("other")));
        settings::set_default_connection(home.path(), Some("a")).unwrap();

        let agent_a = agent(home.path(), start()).await.unwrap();
        let meta = agent_a.session.meta();
        assert_eq!(
            (meta.connection.as_str(), meta.model.as_str()),
            ("a", "from-settings")
        );
        assert_eq!(meta.audience, "terminal");
        assert!(agent_a.session.system().starts_with("You are Tiphys"));
        assert_eq!(agent_a.session.tools().len(), 5);
        assert!(agent_a.local);

        let chosen = Start {
            connection: Some("b".into()),
            model: Some("picked".into()),
            ..start()
        };
        let agent_b = agent(home.path(), chosen).await.unwrap();
        let meta = agent_b.session.meta();
        assert_eq!(
            (meta.connection.as_str(), meta.model.as_str()),
            ("b", "picked")
        );
    }

    #[tokio::test]
    async fn resuming_opens_the_latest_session_or_the_one_named() {
        let home = tempfile::tempdir().unwrap();
        save(home.path(), "a", &connection(Some("m")));
        // Nothing to resume yet: a new one.
        let first = agent(
            home.path(),
            Start {
                resume: Resume::Latest,
                ..start()
            },
        )
        .await
        .unwrap();
        let first_id = first.session.meta().id.clone();
        let second = agent(home.path(), start()).await.unwrap();
        let second_id = second.session.meta().id.clone();
        assert_ne!(first_id, second_id);

        let latest = agent(
            home.path(),
            Start {
                resume: Resume::Latest,
                ..start()
            },
        )
        .await
        .unwrap();
        assert_eq!(latest.session.meta().id, second_id);
        let named = agent(
            home.path(),
            Start {
                resume: Resume::Id(first_id.clone()),
                ..start()
            },
        )
        .await
        .unwrap();
        assert_eq!(named.session.meta().id, first_id);

        // The session's connection was removed since.
        settings::remove_connection(home.path(), "a").unwrap();
        let message = refusal(
            home.path(),
            Start {
                resume: Resume::Latest,
                ..start()
            },
        )
        .await;
        assert!(message.contains("no longer set up"), "{message}");
    }
}
