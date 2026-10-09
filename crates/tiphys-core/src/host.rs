//! The agent's host: what a client talks to.
//!
//! A host takes [`Request`]s one at a time and answers with [`Event`]s. It
//! owns the agent, so it is the one place that starts sessions, runs turns
//! and changes what is configured. The terminal app runs a host inside its
//! own process; the daemon will run one per session, and the app will reach
//! it over a socket with these same requests and events.
//!
//! While a turn runs the host keeps listening: a cancel stops the turn at
//! once, and anything else waits until the turn is over.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::agent::Agent;
use crate::config;
use crate::llm::{Connect, Model, Provider, catalog, check};
use crate::proto::{ConnectionInfo, Draft, Event, Request, SessionInfo, State};
use crate::start::{self, Start};
use crate::{Error, Result, keys, settings};

/// Where a host's events go.
pub type Sink = Arc<dyn Fn(&Event) + Send + Sync>;

pub struct Host {
    home: PathBuf,
    user_home: PathBuf,
    /// Who sessions begun here are with.
    audience: String,
    connect: Arc<dyn Connect>,
    emit: Sink,
    agent: Option<Agent>,
    /// The model list of the connection last tried, kept so that saving the
    /// connection does not have to fetch it again.
    tried: Option<(String, Vec<Model>)>,
}

impl Host {
    pub fn new(
        home: &Path,
        user_home: &Path,
        audience: &str,
        connect: Arc<dyn Connect>,
        emit: Sink,
    ) -> Self {
        Self {
            home: home.to_path_buf(),
            user_home: user_home.to_path_buf(),
            audience: audience.to_string(),
            connect,
            emit,
            agent: None,
            tried: None,
        }
    }

    /// Serves requests until the client goes away.
    pub async fn run(mut self, mut requests: UnboundedReceiver<Request>) {
        let mut waiting = VecDeque::new();
        loop {
            let request = match waiting.pop_front() {
                Some(request) => request,
                None => match requests.recv().await {
                    Some(request) => request,
                    None => return,
                },
            };
            let outcome = match request {
                Request::Prompt { text } => {
                    if !self.turn(&text, &mut requests, &mut waiting).await {
                        return;
                    }
                    Ok(())
                }
                other => self.handle(other).await,
            };
            if let Err(e) = outcome {
                (self.emit)(&Event::Failed {
                    message: e.to_string(),
                });
            }
        }
    }

    /// Runs a turn while still listening. Returns false if the client went
    /// away, after stopping the turn cleanly.
    async fn turn(
        &mut self,
        text: &str,
        requests: &mut UnboundedReceiver<Request>,
        waiting: &mut VecDeque<Request>,
    ) -> bool {
        if self.agent.is_none() {
            let start = Start {
                audience: self.audience.clone(),
                ..Start::default()
            };
            match start::agent_for(&self.home, &self.user_home, start, self.connect.as_ref()).await
            {
                Ok(agent) => self.agent = Some(agent),
                Err(e) => {
                    (self.emit)(&Event::Failed {
                        message: e.to_string(),
                    });
                    return true;
                }
            }
            (self.emit)(&Event::State(self.state()));
        }
        let Some(agent) = self.agent.as_mut() else {
            return true;
        };
        let cancel = agent.cancel.clone();
        let emit = self.emit.clone();
        let turn = agent.turn(text, emit.as_ref());
        tokio::pin!(turn);
        let mut connected = true;
        loop {
            tokio::select! {
                _ = &mut turn => return connected,
                request = requests.recv(), if connected => match request {
                    Some(Request::Cancel) => cancel.cancel(),
                    Some(other) => waiting.push_back(other),
                    // Nobody is left to read the answer. Stop, and let the
                    // turn finish its bookkeeping.
                    None => {
                        cancel.cancel();
                        connected = false;
                    }
                },
            }
        }
    }

    async fn handle(&mut self, request: Request) -> Result<()> {
        match request {
            Request::Hello => {}
            // Handled by `run`, which can listen while the turn goes on.
            Request::Prompt { .. } => {}
            // Nothing is running, so there is nothing to stop.
            Request::Cancel => return Ok(()),
            Request::NewSession => self.agent = None,
            Request::TryConnection(draft) => {
                let models = self.provider_for(&draft)?.models().await?;
                self.tried = Some((draft.name, models.clone()));
                (self.emit)(&Event::Models { models });
                return Ok(());
            }
            Request::CheckModel(draft) => {
                let model = draft.connection.model.clone().unwrap_or_default();
                let checked = match self.provider_for(&draft) {
                    Ok(provider) => check::tool_round_trip(provider.as_ref(), &model).await,
                    Err(e) => Err(e),
                };
                (self.emit)(&Event::ModelChecked {
                    model,
                    ok: checked.is_ok(),
                    message: checked.err().map(|e| e.to_string()).unwrap_or_default(),
                });
                return Ok(());
            }
            Request::SaveConnection(draft) => {
                settings::save_connection(&self.home, &draft.name, &draft.connection)?;
                if let Some(key) = &draft.key {
                    keys::store(&self.home, &draft.name, key)?;
                }
                settings::set_default_connection(&self.home, Some(&draft.name))?;
                if let Some((name, models)) = self.tried.take()
                    && name == draft.name
                {
                    catalog::store(&self.home, &name, &models)?;
                }
                self.agent = None;
            }
            Request::Models { connection } => {
                let config = config::load_at(&self.home)?;
                let found = config.connections.get(&connection).ok_or_else(|| {
                    Error::Config(format!("there is no connection named `{connection}`"))
                })?;
                let key = keys::resolve(&self.home, &connection, found.env_key.as_deref())?;
                let models = self.connect.provider(found, key.as_ref())?.models().await?;
                catalog::store(&self.home, &connection, &models)?;
                (self.emit)(&Event::Models { models });
                return Ok(());
            }
            Request::ChooseModel { connection, model } => {
                settings::set_model(&self.home, &connection, &model)?;
                settings::set_default_connection(&self.home, Some(&connection))?;
                self.agent = None;
            }
        }
        (self.emit)(&Event::State(self.state()));
        Ok(())
    }

    /// A provider for a connection that is being set up. The key is the one
    /// just typed, or else the one already stored under the draft's name.
    fn provider_for(&self, draft: &Draft) -> Result<Arc<dyn Provider>> {
        config::valid_name(&draft.name)?;
        draft
            .connection
            .validate()
            .map_err(|e| Error::Config(format!("connection `{}`: {e}", draft.name)))?;
        let stored;
        let key = match &draft.key {
            Some(key) => Some(key),
            None => {
                // A connection the owner keys through a variable keeps doing
                // so while it is edited, though a draft does not carry that.
                let configured = config::load_at(&self.home)
                    .ok()
                    .and_then(|config| config.connections.get(&draft.name)?.env_key.clone());
                let env_key = draft
                    .connection
                    .env_key
                    .as_deref()
                    .or(configured.as_deref());
                stored = keys::resolve(&self.home, &draft.name, env_key)?;
                stored.as_ref()
            }
        };
        self.connect.provider(&draft.connection, key)
    }

    /// Where things stand. A configuration that cannot be read is reported as
    /// having no connections; the request that needs one says why.
    fn state(&self) -> State {
        let config = config::load_at(&self.home).unwrap_or_default();
        let connections = config
            .connections
            .iter()
            .map(|(name, connection)| ConnectionInfo {
                name: name.clone(),
                base_url: connection.base().to_string(),
                model: connection.model.clone(),
                local: connection.local,
                has_key: keys::resolve(&self.home, name, connection.env_key.as_deref())
                    .is_ok_and(|key| key.is_some()),
            })
            .collect();
        State {
            connections,
            default: config
                .starting_connection()
                .map(|(name, _)| name.to_string()),
            session: self.agent.as_ref().map(|agent| {
                let meta = agent.session.meta();
                SessionInfo {
                    id: meta.id.clone(),
                    connection: meta.connection.clone(),
                    model: meta.model.clone(),
                }
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Connection;
    use crate::keys::Secret;
    use crate::llm::{Delta, DeltaStream, ReplayProvider, ToolCallPart};
    use crate::proto::StopReason;
    use crate::spend::Rates;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

    /// Hands out one scripted provider, and remembers the key it was given.
    struct Scripted {
        provider: Arc<dyn Provider>,
        keys: Mutex<Vec<Option<String>>>,
    }

    impl Connect for Scripted {
        fn provider(&self, _: &Connection, key: Option<&Secret>) -> Result<Arc<dyn Provider>> {
            self.keys
                .lock()
                .unwrap()
                .push(key.map(|k| k.expose().to_string()));
            Ok(self.provider.clone())
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        requests: UnboundedSender<Request>,
        events: tokio::sync::mpsc::UnboundedReceiver<Event>,
        connect: Arc<Scripted>,
        host: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        fn home(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn send(&self, request: Request) {
            self.requests.send(request).unwrap();
        }

        /// The next event that is not a piece of a streaming reply.
        async fn next(&mut self) -> Event {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(10), self.events.recv())
                    .await
                    .expect("the host went quiet")
                    .expect("the host stopped");
                if !matches!(event, Event::Text { .. } | Event::Reasoning { .. }) {
                    return event;
                }
            }
        }

        /// Reads events up to and including the end of a turn.
        async fn turn_end(&mut self) -> (StopReason, Vec<Event>) {
            let mut seen = Vec::new();
            loop {
                let event = self.next().await;
                if let Event::TurnFinished { reason, .. } = &event {
                    return (*reason, seen);
                }
                seen.push(event);
            }
        }
    }

    fn fixture(provider: Arc<dyn Provider>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let (requests, inbox) = unbounded_channel();
        let (outbox, events) = unbounded_channel();
        let connect = Arc::new(Scripted {
            provider,
            keys: Mutex::default(),
        });
        let sink: Sink = Arc::new(move |event| {
            let _ = outbox.send(event.clone());
        });
        let host = Host::new(
            &dir.path().join("state"),
            &dir.path().join("user"),
            "terminal",
            connect.clone(),
            sink,
        );
        Fixture {
            host: tokio::spawn(host.run(inbox)),
            dir,
            requests,
            events,
            connect,
        }
    }

    fn draft(key: Option<&str>, model: Option<&str>) -> Draft {
        Draft {
            name: "work".into(),
            connection: Connection {
                base_url: "https://api.example.com/v1".into(),
                model: model.map(Into::into),
                env_key: None,
                local: false,
            },
            key: key.and_then(Secret::new),
        }
    }

    fn says(text: &str) -> Vec<Delta> {
        vec![Delta::Text(text.into()), Delta::Done]
    }

    fn pings() -> Vec<Delta> {
        vec![
            Delta::ToolCall(ToolCallPart {
                slot: Some(0),
                id: "c1".into(),
                name: "ping".into(),
                arguments: r#"{"word":"anchor"}"#.into(),
            }),
            Delta::Done,
        ]
    }

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            context: None,
            rates: Some(Rates {
                input: 1.0,
                output: 2.0,
                cache_read: None,
                cache_write: None,
            }),
            tools: None,
        }
    }

    #[tokio::test]
    async fn a_first_run_has_nothing_set_up() {
        let mut f = fixture(Arc::new(ReplayProvider::default()));
        f.send(Request::Hello);
        assert_eq!(f.next().await, Event::State(State::default()));

        // A prompt with no connection says where to make one.
        f.send(Request::Prompt { text: "hi".into() });
        let Event::Failed { message } = f.next().await else {
            panic!("expected a failure");
        };
        assert!(message.contains("add one in the app"), "{message}");
    }

    #[tokio::test]
    async fn a_connection_is_tried_checked_saved_and_then_used() {
        let provider = ReplayProvider::new(vec![pings(), says("pong"), says("Hello.")])
            .with_models(vec![model("vendor/model")]);
        let mut f = fixture(Arc::new(provider));

        f.send(Request::TryConnection(draft(Some("sk-typed"), None)));
        assert_eq!(
            f.next().await,
            Event::Models {
                models: vec![model("vendor/model")]
            }
        );

        f.send(Request::CheckModel(draft(
            Some("sk-typed"),
            Some("vendor/model"),
        )));
        assert_eq!(
            f.next().await,
            Event::ModelChecked {
                model: "vendor/model".into(),
                ok: true,
                message: String::new()
            }
        );

        f.send(Request::SaveConnection(draft(
            Some("sk-typed"),
            Some("vendor/model"),
        )));
        let Event::State(state) = f.next().await else {
            panic!("expected the state");
        };
        assert_eq!(state.default.as_deref(), Some("work"));
        assert_eq!(
            state.connections,
            [ConnectionInfo {
                name: "work".into(),
                base_url: "https://api.example.com/v1".into(),
                model: Some("vendor/model".into()),
                local: false,
                has_key: true,
            }]
        );
        assert!(state.session.is_none());
        // The key is on disk, private, and the model list was kept.
        let stored = keys::resolve_with(&f.home(), "work", None, |_| None)
            .unwrap()
            .unwrap();
        assert_eq!(stored.expose(), "sk-typed");
        assert_eq!(
            catalog::load(&f.home(), "work").unwrap().models,
            [model("vendor/model")]
        );

        f.send(Request::Prompt { text: "hi".into() });
        let Event::State(state) = f.next().await else {
            panic!("expected the state");
        };
        let session = state.session.unwrap();
        assert_eq!(
            (session.connection.as_str(), session.model.as_str()),
            ("work", "vendor/model")
        );
        let (reason, seen) = f.turn_end().await;
        assert_eq!(reason, StopReason::Completed);
        assert!(seen.contains(&Event::AssistantMessage {
            text: "Hello.".into()
        }));

        // Every provider was made with the typed key, then the stored one.
        let keys = f.connect.keys.lock().unwrap().clone();
        assert_eq!(keys, vec![Some("sk-typed".to_string()); 3]);
    }

    #[tokio::test]
    async fn a_check_that_fails_says_why_and_saves_nothing() {
        let provider = ReplayProvider::new(vec![says("I cannot use tools.")]);
        let mut f = fixture(Arc::new(provider));
        f.send(Request::CheckModel(draft(
            Some("sk-typed"),
            Some("vendor/model"),
        )));
        let Event::ModelChecked { ok, message, .. } = f.next().await else {
            panic!("expected a check result");
        };
        assert!(
            !ok && message.contains("instead of calling the tool"),
            "{message}"
        );
        assert!(!f.home().join("settings.toml").exists());
        assert!(!f.home().join("keys").exists());

        // A draft that is not usable is refused before anything is contacted.
        let mut bad = draft(None, None);
        bad.name = "Not A Name".into();
        f.send(Request::TryConnection(bad));
        assert!(matches!(f.next().await, Event::Failed { .. }));
        assert_eq!(f.connect.keys.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_new_session_and_a_model_change_start_afresh() {
        let provider = ReplayProvider::new(vec![says("One."), says("Two."), says("Three.")])
            .with_models(vec![model("a"), model("b")]);
        let mut f = fixture(Arc::new(provider));
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;

        let mut session_ids = Vec::new();
        for step in 0..3 {
            match step {
                1 => {
                    f.send(Request::NewSession);
                    assert!(matches!(
                        f.next().await,
                        Event::State(State { session: None, .. })
                    ));
                }
                2 => {
                    f.send(Request::Models {
                        connection: "work".into(),
                    });
                    assert_eq!(
                        f.next().await,
                        Event::Models {
                            models: vec![model("a"), model("b")]
                        }
                    );
                    f.send(Request::ChooseModel {
                        connection: "work".into(),
                        model: "b".into(),
                    });
                    assert!(matches!(
                        f.next().await,
                        Event::State(State { session: None, .. })
                    ));
                }
                _ => {}
            }
            f.send(Request::Prompt {
                text: format!("turn {step}"),
            });
            let Event::State(state) = f.next().await else {
                panic!("expected the state");
            };
            let session = state.session.unwrap();
            assert_eq!(session.model, if step == 2 { "b" } else { "a" });
            session_ids.push(session.id);
            f.turn_end().await;
        }
        session_ids.dedup();
        assert_eq!(session_ids.len(), 3);
    }

    /// A provider whose reply starts and never ends.
    struct Hanging;

    #[async_trait::async_trait]
    impl Provider for Hanging {
        async fn stream(&self, _: crate::llm::Request) -> Result<DeltaStream> {
            use futures_util::StreamExt;
            let start = futures_util::stream::iter(vec![Ok(Delta::Text("thinking".into()))]);
            Ok(Box::pin(start.chain(futures_util::stream::pending())))
        }
        async fn models(&self) -> Result<Vec<Model>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_cancel_stops_the_turn_and_what_arrived_meanwhile_is_dealt_with_after() {
        let mut f = fixture(Arc::new(Hanging));
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;

        f.send(Request::Prompt { text: "hi".into() });
        assert!(matches!(f.next().await, Event::State(_)));
        assert_eq!(f.next().await, Event::UserMessage { text: "hi".into() });
        // Sent during the turn: it waits. Then the cancel, which does not.
        f.send(Request::Hello);
        f.send(Request::Cancel);
        let (reason, seen) = f.turn_end().await;
        assert_eq!(reason, StopReason::Cancelled);
        assert!(seen.is_empty(), "{seen:?}");
        assert!(matches!(
            f.next().await,
            Event::State(State {
                session: Some(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn the_host_stops_when_its_client_goes_away_even_during_a_turn() {
        let mut f = fixture(Arc::new(Hanging));
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt { text: "hi".into() });
        assert!(matches!(f.next().await, Event::State(_)));
        assert_eq!(f.next().await, Event::UserMessage { text: "hi".into() });

        drop(f.requests);
        tokio::time::timeout(Duration::from_secs(10), f.host)
            .await
            .unwrap()
            .unwrap();
    }
}
