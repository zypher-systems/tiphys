//! The agent's host: what clients talk to.
//!
//! A host owns one audience's agent. It is the one place that starts
//! sessions, runs turns and changes what is configured. Clients come and go:
//! a client that attaches is sent where things stand and the conversation so
//! far, and from then on everything that happens. The terminal app is a
//! client; so is a one-shot run, and later a chat adapter. The app can run a
//! host inside its own process, or reach one in the daemon over a socket,
//! with the same requests and events either way.
//!
//! What happens in a turn goes to every client. The answer to a client's own
//! question, such as a model list, goes to that client alone.
//!
//! While a turn runs the host keeps listening: a cancel or an approval is
//! dealt with at once, a client can attach or leave, and anything else waits
//! until the turn is over.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::agent::Agent;
use crate::approval::{Decision, Pending};
use crate::llm::{Connect, Model, Provider, catalog, check};
use crate::proto::{ConnectionInfo, Draft, Event, Request, SessionInfo, State};
use crate::start::{self, Resume, Start};
use crate::{Error, Result, config, keys, session, settings};

/// Where a client's events go.
pub type Sink = Arc<dyn Fn(&Event) + Send + Sync>;

/// Identifies a client to the host it is attached to.
pub type ClientId = u64;

/// What reaches a host.
pub enum Input {
    /// A client came. It is sent the state and the conversation so far, and
    /// then everything that happens.
    Attach { client: ClientId, sink: Sink },
    /// A client left. The host and any turn it is running carry on.
    Detach { client: ClientId },
    /// A client asked for something.
    Request { client: ClientId, request: Request },
}

type Clients = Arc<Mutex<Vec<(ClientId, Sink)>>>;

pub struct Host {
    home: PathBuf,
    user_home: PathBuf,
    /// Who sessions begun here are with.
    audience: String,
    connect: Arc<dyn Connect>,
    clients: Clients,
    /// The questions waiting on a client for a yes or a no.
    approvals: Arc<Pending>,
    agent: Option<Agent>,
    /// The owner asked for a new session, so the last one is not picked up
    /// again; the next message begins one.
    fresh: bool,
    /// The model list of the connection last tried, kept so that saving the
    /// connection does not have to fetch it again.
    tried: Option<(String, Vec<Model>)>,
}

impl Host {
    pub fn new(home: &Path, user_home: &Path, audience: &str, connect: Arc<dyn Connect>) -> Self {
        Self {
            home: home.to_path_buf(),
            user_home: user_home.to_path_buf(),
            audience: audience.to_string(),
            connect,
            clients: Clients::default(),
            approvals: Arc::new(Pending::default()),
            agent: None,
            fresh: false,
            tried: None,
        }
    }

    /// Serves until nothing can reach it any more. A turn that is running
    /// when that happens is stopped cleanly first.
    pub async fn run(mut self, mut inbox: UnboundedReceiver<Input>) {
        let mut waiting = VecDeque::new();
        loop {
            let input = match waiting.pop_front() {
                Some(input) => input,
                None => match inbox.recv().await {
                    Some(input) => input,
                    None => return,
                },
            };
            match input {
                Input::Attach { client, sink } => self.attach(client, sink).await,
                Input::Detach { client } => detach(&self.clients, client),
                Input::Request {
                    client,
                    request: Request::Prompt { text },
                } => {
                    if !self.turn(client, &text, &mut inbox, &mut waiting).await {
                        return;
                    }
                }
                Input::Request { client, request } => {
                    if let Err(e) = self.handle(client, request).await {
                        self.reply(
                            client,
                            &Event::Failed {
                                message: e.to_string(),
                            },
                        );
                    }
                }
            }
        }
    }

    /// Takes a client in: picks up the audience's last session if none is
    /// open, then tells the client where things stand and what has been said.
    async fn attach(&mut self, client: ClientId, sink: Sink) {
        if self.agent.is_none() && !self.fresh {
            // A session that cannot be reopened, because its connection is
            // gone, say, is left where it is. The next message starts another.
            if let Ok(Some(id)) = session::latest(&self.home, &self.audience)
                && let Ok(agent) = self.start(Resume::Id(id)).await
            {
                self.agent = Some(agent);
            }
        }
        sink(&Event::State(self.state()));
        sink(&Event::History {
            events: self.history(),
        });
        self.clients.lock().unwrap().push((client, sink));
    }

    async fn start(&self, resume: Resume) -> Result<Agent> {
        let start = Start {
            audience: self.audience.clone(),
            resume,
            ..Start::default()
        };
        start::agent_for(
            &self.home,
            &self.user_home,
            start,
            self.connect.as_ref(),
            self.approvals.clone(),
        )
        .await
    }

    /// Runs a turn while still listening. Returns false if nothing can reach
    /// the host any more, after stopping the turn cleanly.
    async fn turn(
        &mut self,
        client: ClientId,
        text: &str,
        inbox: &mut UnboundedReceiver<Input>,
        waiting: &mut VecDeque<Input>,
    ) -> bool {
        if self.agent.is_none() {
            match self.start(Resume::New).await {
                Ok(agent) => self.agent = Some(agent),
                Err(e) => {
                    self.reply(
                        client,
                        &Event::Failed {
                            message: e.to_string(),
                        },
                    );
                    return true;
                }
            }
            self.fresh = false;
            self.broadcast(&Event::State(self.state()));
        }
        // A client that attaches part-way is told these as they are now.
        let state = self.state();
        let (home, clients, approvals) = (
            self.home.clone(),
            self.clients.clone(),
            self.approvals.clone(),
        );
        let Some(agent) = self.agent.as_mut() else {
            return true;
        };
        let session = agent.session.meta().id.clone();
        let cancel = agent.cancel.clone();
        let emit = broadcaster(&clients);
        let turn = agent.turn(text, emit.as_ref());
        tokio::pin!(turn);
        let mut reachable = true;
        loop {
            tokio::select! {
                _ = &mut turn => return reachable,
                input = inbox.recv(), if reachable => match input {
                    Some(Input::Request { request: Request::Cancel, .. }) => cancel.cancel(),
                    Some(Input::Request { request: Request::Approval { id, approve, note }, .. }) => {
                        let decision = if approve {
                            Decision::Approve
                        } else {
                            Decision::Deny(note.unwrap_or_else(|| "the owner said no".into()))
                        };
                        approvals.answer(&id, decision);
                    }
                    // The turn is suspended while this runs, and an event is
                    // written and sent in one step, so what is read here is
                    // exactly what has been sent: nothing is missed and
                    // nothing comes twice.
                    Some(Input::Attach { client, sink }) => {
                        sink(&Event::State(state.clone()));
                        sink(&Event::History { events: session::history(&home, &session) });
                        clients.lock().unwrap().push((client, sink));
                    }
                    Some(Input::Detach { client }) => detach(&clients, client),
                    Some(other) => waiting.push_back(other),
                    // Nobody is left to read the answer. Stop, and let the
                    // turn finish its bookkeeping.
                    None => {
                        cancel.cancel();
                        reachable = false;
                    }
                },
            }
        }
    }

    async fn handle(&mut self, client: ClientId, request: Request) -> Result<()> {
        match request {
            Request::Hello => self.reply(client, &Event::State(self.state())),
            // Handled by `run`, which can listen while the turn goes on.
            Request::Prompt { .. } => {}
            // Nothing is running, so there is nothing to stop or to answer.
            Request::Cancel | Request::Approval { .. } => {}
            Request::NewSession => self.begin_afresh(),
            Request::TryConnection(draft) => {
                let models = self.provider_for(&draft)?.models().await?;
                self.tried = Some((draft.name, models.clone()));
                self.reply(client, &Event::Models { models });
            }
            Request::CheckModel(draft) => {
                let model = draft.connection.model.clone().unwrap_or_default();
                let checked = match self.provider_for(&draft) {
                    Ok(provider) => check::tool_round_trip(provider.as_ref(), &model).await,
                    Err(e) => Err(e),
                };
                self.reply(
                    client,
                    &Event::ModelChecked {
                        model,
                        ok: checked.is_ok(),
                        message: checked.err().map(|e| e.to_string()).unwrap_or_default(),
                    },
                );
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
                self.begin_afresh();
            }
            Request::Models { connection } => {
                let config = config::load_at(&self.home)?;
                let found = config.connections.get(&connection).ok_or_else(|| {
                    Error::Config(format!("there is no connection named `{connection}`"))
                })?;
                let key = keys::resolve(&self.home, &connection, found.env_key.as_deref())?;
                let models = self.connect.provider(found, key.as_ref())?.models().await?;
                catalog::store(&self.home, &connection, &models)?;
                self.reply(client, &Event::Models { models });
            }
            Request::ChooseModel { connection, model } => {
                settings::set_model(&self.home, &connection, &model)?;
                settings::set_default_connection(&self.home, Some(&connection))?;
                self.begin_afresh();
            }
        }
        Ok(())
    }

    /// Leaves the current session. Every client is told, and shown an empty
    /// conversation; the next message begins a new session.
    fn begin_afresh(&mut self) {
        self.agent = None;
        self.fresh = true;
        self.broadcast(&Event::State(self.state()));
        self.broadcast(&Event::History { events: Vec::new() });
    }

    fn broadcast(&self, event: &Event) {
        broadcaster(&self.clients)(event);
    }

    /// Sends an event to one client, if it is still there.
    fn reply(&self, client: ClientId, event: &Event) {
        let sink = self
            .clients
            .lock()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == client)
            .map(|(_, sink)| sink.clone());
        if let Some(sink) = sink {
            sink(event);
        }
    }

    /// What has been said in the open session, for a client that just came.
    fn history(&self) -> Vec<Event> {
        match &self.agent {
            Some(agent) => session::history(&self.home, &agent.session.meta().id),
            None => Vec::new(),
        }
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

/// A sink that sends to every client attached at the moment it is called.
fn broadcaster(clients: &Clients) -> Sink {
    let clients = clients.clone();
    Arc::new(move |event| {
        // Copied out first, so a sink is never called with the list locked.
        let sinks: Vec<Sink> = clients
            .lock()
            .unwrap()
            .iter()
            .map(|(_, sink)| sink.clone())
            .collect();
        for sink in sinks {
            sink(event);
        }
    })
}

fn detach(clients: &Clients, client: ClientId) {
    clients.lock().unwrap().retain(|(id, _)| *id != client);
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

    /// One attached client: what it sends and what it is sent.
    struct Client {
        id: ClientId,
        inbox: UnboundedSender<Input>,
        events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    }

    impl Client {
        /// Attaches a new client and hands it back before it has read anything.
        fn attach(id: ClientId, inbox: &UnboundedSender<Input>) -> Self {
            let (outbox, events) = unbounded_channel();
            let sink: Sink = Arc::new(move |event| {
                let _ = outbox.send(event.clone());
            });
            inbox.send(Input::Attach { client: id, sink }).unwrap();
            Self {
                id,
                inbox: inbox.clone(),
                events,
            }
        }

        fn send(&self, request: Request) {
            self.inbox
                .send(Input::Request {
                    client: self.id,
                    request,
                })
                .unwrap();
        }

        /// The next event of any kind.
        async fn raw(&mut self) -> Event {
            tokio::time::timeout(Duration::from_secs(10), self.events.recv())
                .await
                .expect("the host went quiet")
                .expect("the host stopped")
        }

        /// The next event that is not a piece of a streaming reply or a
        /// replay of the conversation.
        async fn next(&mut self) -> Event {
            loop {
                let event = self.raw().await;
                if !matches!(
                    event,
                    Event::Text { .. } | Event::Reasoning { .. } | Event::History { .. }
                ) {
                    return event;
                }
            }
        }

        /// What a client is sent when it attaches: the state, then the
        /// conversation so far.
        async fn greeting(&mut self) -> (State, Vec<Event>) {
            let Event::State(state) = self.raw().await else {
                panic!("expected the state first");
            };
            let Event::History { events } = self.raw().await else {
                panic!("expected the conversation second");
            };
            (state, events)
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

        /// Whether nothing is waiting to be read, pieces of a streaming
        /// reply and replays of the conversation aside.
        fn has_nothing_waiting(&mut self) -> bool {
            while let Ok(event) = self.events.try_recv() {
                let passing = matches!(
                    event,
                    Event::Text { .. } | Event::Reasoning { .. } | Event::History { .. }
                );
                if !passing {
                    return false;
                }
            }
            true
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        inbox: UnboundedSender<Input>,
        client: Client,
        connect: Arc<Scripted>,
        host: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        fn home(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn send(&self, request: Request) {
            self.client.send(request);
        }

        async fn next(&mut self) -> Event {
            self.client.next().await
        }

        async fn turn_end(&mut self) -> (StopReason, Vec<Event>) {
            self.client.turn_end().await
        }
    }

    /// A host on an empty state directory, with one client attached and past
    /// its greeting.
    async fn fixture(provider: Arc<dyn Provider>) -> Fixture {
        fixture_in(tempfile::tempdir().unwrap(), provider).await.0
    }

    /// A host on `dir`, as after a restart, with client 0 attached. Returns
    /// what that client was greeted with.
    async fn fixture_in(
        dir: tempfile::TempDir,
        provider: Arc<dyn Provider>,
    ) -> (Fixture, (State, Vec<Event>)) {
        let (inbox, requests) = unbounded_channel();
        let connect = Arc::new(Scripted {
            provider,
            keys: Mutex::default(),
        });
        let host = Host::new(
            &dir.path().join("state"),
            &dir.path().join("user"),
            "terminal",
            connect.clone(),
        );
        let host = tokio::spawn(host.run(requests));
        let mut client = Client::attach(0, &inbox);
        let greeting = client.greeting().await;
        (
            Fixture {
                dir,
                inbox,
                client,
                connect,
                host,
            },
            greeting,
        )
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
        let mut f = fixture(Arc::new(ReplayProvider::default())).await;
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
        let mut f = fixture(Arc::new(provider)).await;

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
        let mut f = fixture(Arc::new(provider)).await;
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
        let mut f = fixture(Arc::new(provider)).await;
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
        let mut f = fixture(Arc::new(Hanging)).await;
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
        let mut f = fixture(Arc::new(Hanging)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt { text: "hi".into() });
        assert!(matches!(f.next().await, Event::State(_)));
        assert_eq!(f.next().await, Event::UserMessage { text: "hi".into() });

        drop(f.client.inbox);
        drop(f.inbox);
        tokio::time::timeout(Duration::from_secs(10), f.host)
            .await
            .unwrap()
            .unwrap();
    }

    fn writes_env() -> Vec<Delta> {
        vec![
            Delta::ToolCall(ToolCallPart {
                slot: Some(0),
                id: "w1".into(),
                name: "write_file".into(),
                arguments: r#"{"path":".env","content":"TOKEN=x\n","reason":"to save it"}"#.into(),
            }),
            Delta::Done,
        ]
    }

    #[tokio::test]
    async fn an_action_that_asks_waits_for_the_clients_answer() {
        for approve in [true, false] {
            let provider = ReplayProvider::new(vec![writes_env(), says("Done.")]);
            let mut f = fixture(Arc::new(provider)).await;
            f.send(Request::SaveConnection(draft(None, Some("a"))));
            f.next().await;
            f.send(Request::Prompt {
                text: "save the token".into(),
            });

            let asked = loop {
                if let Event::ApprovalRequested {
                    id, summary, why, ..
                } = f.next().await
                {
                    break (id, summary, why);
                }
            };
            assert_eq!(asked.0, "w1");
            assert!(
                asked.1.starts_with("create ") && asked.2.contains("secret"),
                "{asked:?}"
            );
            // An answer to some other question changes nothing.
            f.send(Request::Approval {
                id: "other".into(),
                approve: true,
                note: None,
            });
            let note = (!approve).then(|| "not now".to_string());
            f.send(Request::Approval {
                id: "w1".into(),
                approve,
                note,
            });

            let (reason, seen) = f.turn_end().await;
            assert_eq!(reason, StopReason::Completed);
            let resolved = seen.iter().find_map(|e| match e {
                Event::ApprovalResolved { approved, note, .. } => Some((*approved, note.clone())),
                _ => None,
            });
            let expected_note = if approve { "" } else { "not now" };
            assert_eq!(resolved, Some((approve, expected_note.to_string())));
            assert_eq!(f.dir.path().join("user/.env").exists(), approve);
        }
    }

    #[tokio::test]
    async fn a_client_that_attaches_is_sent_the_conversation_and_then_what_happens() {
        let provider = ReplayProvider::new(vec![says("One."), says("Two.")]);
        let mut f = fixture(Arc::new(provider)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt {
            text: "first".into(),
        });
        f.turn_end().await;

        // A second client comes late. It is shown the turn it missed.
        let mut late = Client::attach(2, &f.inbox);
        let (state, history) = late.greeting().await;
        assert!(state.session.is_some());
        let kinds: Vec<&str> = history
            .iter()
            .map(|event| match event {
                Event::UserMessage { .. } => "user",
                Event::AssistantMessage { .. } => "assistant",
                Event::Spend { .. } => "spend",
                Event::TurnFinished { .. } => "finished",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "spend", "assistant", "finished"]);
        assert!(history.contains(&Event::AssistantMessage {
            text: "One.".into()
        }));

        // From here on both see what happens, whoever asked.
        late.send(Request::Prompt {
            text: "second".into(),
        });
        for client in [&mut f.client, &mut late] {
            let (reason, seen) = client.turn_end().await;
            assert_eq!(reason, StopReason::Completed);
            assert!(seen.contains(&Event::UserMessage {
                text: "second".into()
            }));
            assert!(seen.contains(&Event::AssistantMessage {
                text: "Two.".into()
            }));
        }
    }

    #[tokio::test]
    async fn the_answer_to_a_clients_own_question_goes_to_that_client_alone() {
        let provider = ReplayProvider::default().with_models(vec![model("a")]);
        let mut f = fixture(Arc::new(provider)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        let mut other = Client::attach(2, &f.inbox);
        other.greeting().await;

        other.send(Request::Models {
            connection: "work".into(),
        });
        assert_eq!(
            other.next().await,
            Event::Models {
                models: vec![model("a")]
            }
        );
        other.send(Request::Models {
            connection: "nope".into(),
        });
        assert!(matches!(other.next().await, Event::Failed { .. }));
        other.send(Request::Hello);
        assert!(matches!(other.next().await, Event::State(_)));
        assert!(f.client.has_nothing_waiting());

        // A change to what is set up is everyone's business.
        other.send(Request::NewSession);
        assert!(matches!(other.next().await, Event::State(_)));
        assert!(matches!(f.next().await, Event::State(_)));
    }

    #[tokio::test]
    async fn a_client_can_attach_in_the_middle_of_a_turn_and_another_can_leave() {
        let mut f = fixture(Arc::new(Hanging)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt { text: "hi".into() });
        assert!(matches!(f.next().await, Event::State(_)));
        assert_eq!(f.next().await, Event::UserMessage { text: "hi".into() });

        // The first client goes away; the turn carries on without it.
        f.inbox.send(Input::Detach { client: 0 }).unwrap();
        let mut second = Client::attach(2, &f.inbox);
        let (state, history) = second.greeting().await;
        assert!(state.session.is_some());
        assert_eq!(history, [Event::UserMessage { text: "hi".into() }]);

        second.send(Request::Cancel);
        let (reason, _) = second.turn_end().await;
        assert_eq!(reason, StopReason::Cancelled);
        // Nothing of that reached the client that had left.
        tokio::task::yield_now().await;
        assert!(f.client.has_nothing_waiting());
    }

    #[tokio::test]
    async fn after_a_restart_the_last_session_is_picked_up_where_it_was() {
        let provider = ReplayProvider::new(vec![says("One.")]);
        let mut f = fixture(Arc::new(provider)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt {
            text: "first".into(),
        });
        let Event::State(state) = f.next().await else {
            panic!("expected the state");
        };
        let session = state.session.unwrap().id;
        f.turn_end().await;

        // The process ends, and another starts on the same directory.
        let Fixture {
            dir,
            inbox,
            client,
            host,
            ..
        } = f;
        drop((inbox, client));
        host.await.unwrap();
        let provider = ReplayProvider::new(vec![says("Two.")]);
        let (mut f, (state, history)) = fixture_in(dir, Arc::new(provider)).await;
        assert_eq!(
            state.session.as_ref().map(|s| s.id.as_str()),
            Some(session.as_str())
        );
        assert!(history.contains(&Event::AssistantMessage {
            text: "One.".into()
        }));

        // The next message carries on in it.
        f.send(Request::Prompt {
            text: "second".into(),
        });
        let (reason, seen) = f.turn_end().await;
        assert_eq!(reason, StopReason::Completed);
        assert!(!seen.iter().any(|event| matches!(event, Event::State(_))));
        assert_eq!(crate::session::list(&f.home()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_turn_that_a_restart_cut_short_is_shown_as_ended() {
        let mut f = fixture(Arc::new(Hanging)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt { text: "hi".into() });
        assert!(matches!(f.next().await, Event::State(_)));
        assert_eq!(f.next().await, Event::UserMessage { text: "hi".into() });

        // The process is killed mid-turn: no clean stop, nothing written.
        let Fixture { dir, host, .. } = f;
        host.abort();
        let _ = host.await;

        let (_, (_, history)) = fixture_in(dir, Arc::new(ReplayProvider::default())).await;
        assert_eq!(
            history.last(),
            Some(&Event::TurnFinished {
                reason: StopReason::Failed,
                error: Some(crate::session::INTERRUPTED.into()),
            })
        );
    }

    #[tokio::test]
    async fn asking_for_a_new_session_means_the_old_one_is_not_picked_up_again() {
        let provider = ReplayProvider::new(vec![says("One."), says("Two.")]);
        let mut f = fixture(Arc::new(provider)).await;
        f.send(Request::SaveConnection(draft(None, Some("a"))));
        f.next().await;
        f.send(Request::Prompt {
            text: "first".into(),
        });
        f.turn_end().await;

        f.send(Request::NewSession);
        assert!(matches!(
            f.next().await,
            Event::State(State { session: None, .. })
        ));
        // A client that comes now is shown an empty conversation, not the old one.
        let mut late = Client::attach(2, &f.inbox);
        let (state, history) = late.greeting().await;
        assert!(state.session.is_none() && history.is_empty());

        f.send(Request::Prompt {
            text: "second".into(),
        });
        f.turn_end().await;
        assert_eq!(crate::session::list(&f.home()).unwrap().len(), 2);
    }
}
