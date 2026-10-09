//! The Tiphys daemon.
//!
//! The daemon is the agent's hosts behind a Unix socket. It is the only
//! process that writes the state directory. Clients connect, say who they
//! are talking as, and are attached to that audience's host; when they go,
//! the host and whatever it is doing carry on.
//!
//! Who may connect is decided twice: by the socket's permissions, which let
//! in its owner and its group, and by asking the kernel which user is on the
//! other end, who has to be the daemon's own user, root, or one of the owners
//! named in the configuration.

#![forbid(unsafe_code)]

pub mod hosts;
pub mod install;
pub mod telegram;

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tiphys_core::host::{Input, Sink};
use tiphys_core::llm::{ChatConnect, Connect};
use tiphys_core::proto::Request;
use tiphys_core::wire::{self, AUDIENCES, ClientFrame, PROTOCOL, ServerFrame};
use tiphys_core::{Error, Result, config, lock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;

use crate::hosts::Hosts;
use crate::telegram::{Adapter, Control};

/// How long a client has to say hello.
const HELLO_WITHIN: Duration = Duration::from_secs(10);
/// How long Telegram is given to finish what it is sending when the daemon
/// ends.
const STOP_WITHIN: Duration = Duration::from_secs(5);
/// The socket's permissions: its owner and its group.
const SOCKET_MODE: u32 = 0o660;
/// The longest path a Unix socket can have on Linux.
const SOCKET_PATH_MAX: usize = 107;

/// Runs the daemon for the state directory `home` until it is told to stop
/// with SIGTERM or SIGINT.
pub async fn run(home: &Path, user_home: &Path) -> Result<()> {
    // Held until this returns: one host per state directory.
    let _lock = lock::take(home)?;
    let socket = wire::listen_path(home);
    let listener = bind(&socket)?;
    let daemon = Daemon::new(home, user_home, Arc::new(ChatConnect))?;
    println!(
        "Tiphys {} is listening on {}",
        tiphys_core::VERSION,
        socket.display()
    );

    let outcome = daemon.serve(listener, stop_signal()).await;
    let _ = std::fs::remove_file(&socket);
    outcome
}

/// Binds the socket. The caller holds the instance lock, so a socket file
/// already there belongs to a daemon that is gone.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    let io = |e: std::io::Error| Error::Io(format!("{}: {e}", socket.display()));
    // The kernel's limit on the length of a socket's path is short, and its
    // own error for going over it does not say what to do.
    let length = socket.as_os_str().len();
    if length > SOCKET_PATH_MAX {
        return Err(Error::Config(format!(
            "the socket's path, {}, is {length} bytes long and the system allows {SOCKET_PATH_MAX}; \
             set {} to a shorter path",
            socket.display(),
            wire::SOCKET_ENV
        )));
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let listener = UnixListener::bind(socket).map_err(io)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(SOCKET_MODE)).map_err(io)?;
    Ok(listener)
}

/// Resolves when the process is asked to stop.
async fn stop_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut terminate), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // Without signal handlers the daemon runs until it is killed.
        return std::future::pending().await;
    };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
}

pub struct Daemon {
    home: PathBuf,
    user_home: PathBuf,
    connect: Arc<dyn Connect>,
    /// The users who may connect.
    owners: Vec<u32>,
}

/// What every connection needs.
struct Shared {
    hosts: Arc<Hosts>,
    owners: Vec<u32>,
    /// The way to ask the Telegram adapter something.
    telegram: UnboundedSender<Control>,
}

impl Daemon {
    pub fn new(home: &Path, user_home: &Path, connect: Arc<dyn Connect>) -> Result<Self> {
        let mut owners = config::load_at(home)?.daemon.owners;
        owners.push(rustix::process::getuid().as_raw());
        owners.push(0);
        Ok(Self {
            home: home.to_path_buf(),
            user_home: user_home.to_path_buf(),
            connect,
            owners,
        })
    }

    /// Serves clients on `listener` until `stop` resolves, then stops any
    /// running turn cleanly and returns.
    pub async fn serve(self, listener: UnixListener, stop: impl Future<Output = ()>) -> Result<()> {
        let hosts = Arc::new(Hosts::new(
            &self.home,
            &self.user_home,
            self.connect.clone(),
        ));
        let (telegram, asked) = unbounded_channel();
        let mut adapter = tokio::spawn(Adapter::new(hosts.clone()).run(asked));
        let shared = Arc::new(Shared {
            hosts: hosts.clone(),
            owners: self.owners,
            telegram,
        });

        let mut connections = JoinSet::new();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => break,
                accepted = listener.accept() => {
                    // A client that fails to connect is its own problem; the
                    // daemon goes on accepting.
                    if let Ok((stream, _)) = accepted {
                        connections.spawn(serve_client(stream, shared.clone()));
                    }
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        // Clients and Telegram first, so that nothing still holds a way to
        // reach a host. A host with nothing that can reach it stops its turn
        // and ends.
        connections.shutdown().await;
        drop(shared);
        if tokio::time::timeout(STOP_WITHIN, &mut adapter)
            .await
            .is_err()
        {
            adapter.abort();
            let _ = adapter.await;
        }
        hosts.stop().await;
        Ok(())
    }
}

/// Serves one client from its hello to its leaving.
async fn serve_client(stream: UnixStream, shared: Arc<Shared>) {
    let peer = stream.peer_cred().map(|cred| cred.uid());
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut refuse = async |message: String| {
        let _ = write
            .write_all(wire::line(&ServerFrame::Refused { message }).as_bytes())
            .await;
    };

    match peer {
        Ok(uid) if shared.owners.contains(&uid) => {}
        Ok(uid) => return refuse(format!("user {uid} is not an owner of this Tiphys")).await,
        Err(e) => return refuse(format!("the daemon could not tell who is connecting: {e}")).await,
    }
    let hello = tokio::time::timeout(HELLO_WITHIN, lines.next_line()).await;
    let Ok(Ok(Some(hello))) = hello else {
        return;
    };
    let (protocol, audience) = match serde_json::from_str::<ClientFrame>(&hello) {
        Ok(ClientFrame::Hello {
            protocol, audience, ..
        }) => (protocol, audience),
        _ => return refuse("the first thing a client sends has to be its hello".into()).await,
    };
    if protocol != PROTOCOL {
        return refuse(format!(
            "this client speaks protocol {protocol} and the daemon speaks {PROTOCOL}; they have \
             to be the same version of Tiphys"
        ))
        .await;
    }
    if !AUDIENCES.contains(&audience.as_str()) {
        return refuse(format!(
            "`{audience}` is not something a client can talk as"
        ))
        .await;
    }
    let inbox = shared.hosts.inbox(&audience);
    let hello = ServerFrame::Hello {
        protocol: PROTOCOL,
        version: tiphys_core::VERSION.to_string(),
    };
    if write
        .write_all(wire::line(&hello).as_bytes())
        .await
        .is_err()
    {
        return;
    }

    let client = shared.hosts.client_id();
    let (outbox, mut outgoing) = unbounded_channel::<String>();
    let sink: Sink = Arc::new(move |event| {
        let _ = outbox.send(wire::event_line(event));
    });
    let attach = Input::Attach {
        client,
        sink: sink.clone(),
    };
    if inbox.send(attach).is_err() {
        return;
    }
    loop {
        tokio::select! {
            line = outgoing.recv() => {
                let Some(line) = line else { break };
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                match serde_json::from_str::<ClientFrame>(&line) {
                    // Telegram is the daemon's, not any one host's.
                    Ok(ClientFrame::Request { request: Request::Telegram(request) }) => {
                        let reply = sink.clone();
                        if shared.telegram.send(Control { request, reply }).is_err() {
                            break;
                        }
                    }
                    Ok(ClientFrame::Request { request }) => {
                        if inbox.send(Input::Request { client, request }).is_err() {
                            break;
                        }
                    }
                    // A second hello, or something that is not a frame: the
                    // client is not one this daemon can talk to.
                    _ => break,
                }
            }
        }
    }
    let _ = inbox.send(Input::Detach { client });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::mpsc::RecvTimeoutError;
    use tiphys_core::client::{self, Client};
    use tiphys_core::config::Connection;
    use tiphys_core::keys::Secret;
    use tiphys_core::llm::{Delta, DeltaStream, Model, Provider, ReplayProvider};
    use tiphys_core::proto::{Draft, Event, Request, StopReason};
    use tiphys_core::wire::{ONESHOT, TERMINAL};
    use tokio::sync::oneshot;

    struct Scripted(Arc<dyn Provider>);

    impl Connect for Scripted {
        fn provider(&self, _: &Connection, _: Option<&Secret>) -> Result<Arc<dyn Provider>> {
            Ok(self.0.clone())
        }
    }

    /// A provider whose reply starts and never ends.
    struct Hanging;

    #[async_trait]
    impl Provider for Hanging {
        async fn stream(&self, _: tiphys_core::llm::Request) -> Result<DeltaStream> {
            use futures_util::StreamExt;
            let start = futures_util::stream::iter(vec![Ok(Delta::Text("thinking".into()))]);
            Ok(Box::pin(start.chain(futures_util::stream::pending())))
        }
        async fn models(&self) -> Result<Vec<Model>> {
            Ok(Vec::new())
        }
    }

    struct Running {
        dir: tempfile::TempDir,
        socket: PathBuf,
        stop: Option<oneshot::Sender<()>>,
        daemon: tokio::task::JoinHandle<Result<()>>,
    }

    impl Running {
        async fn stop(mut self) -> tempfile::TempDir {
            let _ = self.stop.take().unwrap().send(());
            tokio::time::timeout(Duration::from_secs(20), self.daemon)
                .await
                .expect("the daemon did not stop")
                .unwrap()
                .unwrap();
            self.dir
        }
    }

    async fn start_in(dir: tempfile::TempDir, provider: Arc<dyn Provider>) -> Running {
        let socket = dir.path().join("tiphys.sock");
        let listener = bind(&socket).unwrap();
        let daemon = Daemon::new(
            &dir.path().join("state"),
            &dir.path().join("user"),
            Arc::new(Scripted(provider)),
        )
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let daemon = tokio::spawn(daemon.serve(listener, async {
            let _ = stopped.await;
        }));
        Running {
            dir,
            socket,
            stop: Some(stop),
            daemon,
        }
    }

    async fn start(provider: Arc<dyn Provider>) -> Running {
        start_in(tempfile::tempdir().unwrap(), provider).await
    }

    /// The blocking client, used from a test that is async.
    async fn connect(socket: &Path, audience: &'static str) -> Result<Client> {
        let socket = socket.to_path_buf();
        tokio::task::spawn_blocking(move || client::connect(&socket, audience))
            .await
            .unwrap()
    }

    /// The next event that is not a piece of a streaming reply.
    async fn next(client: Client) -> (Client, Option<Event>) {
        tokio::task::spawn_blocking(move || {
            loop {
                match client.events.recv_timeout(Duration::from_secs(10)) {
                    Ok(Event::Text { .. } | Event::Reasoning { .. }) => {}
                    Ok(event) => return (client, Some(event)),
                    Err(RecvTimeoutError::Disconnected) => return (client, None),
                    Err(RecvTimeoutError::Timeout) => panic!("the daemon went quiet"),
                }
            }
        })
        .await
        .unwrap()
    }

    /// Reads until an event matches, and returns it.
    async fn until(mut client: Client, wanted: impl Fn(&Event) -> bool) -> (Client, Event) {
        loop {
            let (back, event) = next(client).await;
            client = back;
            let event = event.expect("the connection closed");
            if wanted(&event) {
                return (client, event);
            }
        }
    }

    fn draft() -> Draft {
        Draft {
            name: "work".into(),
            connection: Connection {
                base_url: "https://api.example.com/v1".into(),
                model: Some("m".into()),
                env_key: None,
                local: true,
            },
            key: Secret::new("sk-typed"),
        }
    }

    fn says(text: &str) -> Vec<Delta> {
        vec![Delta::Text(text.into()), Delta::Done]
    }

    #[tokio::test]
    async fn a_client_sets_up_a_connection_and_runs_a_turn_over_the_socket() {
        let running = start(Arc::new(ReplayProvider::new(vec![says("Hello.")]))).await;
        assert_eq!(
            std::fs::metadata(&running.socket)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o660
        );

        let client = connect(&running.socket, TERMINAL).await.unwrap();
        // The greeting: where things stand, then the conversation so far.
        let (client, state) = next(client).await;
        assert!(
            matches!(state, Some(Event::State(ref s)) if s.connections.is_empty()),
            "{state:?}"
        );
        let (client, history) = next(client).await;
        assert_eq!(history, Some(Event::History { events: Vec::new() }));

        client.send(Request::SaveConnection(draft())).unwrap();
        let (client, saved) = until(client, |e| matches!(e, Event::State(_))).await;
        let Event::State(state) = saved else {
            unreachable!()
        };
        assert!(state.connections[0].has_key);
        // The typed key crossed the socket and is stored on the daemon's side.
        let key = std::fs::read_to_string(running.dir.path().join("state/keys/work")).unwrap();
        assert_eq!(key, "sk-typed");

        client.send(Request::Prompt { text: "hi".into() }).unwrap();
        let (client, said) = until(client, |e| matches!(e, Event::AssistantMessage { .. })).await;
        assert_eq!(
            said,
            Event::AssistantMessage {
                text: "Hello.".into()
            }
        );
        let (_, finished) = until(client, |e| matches!(e, Event::TurnFinished { .. })).await;
        assert_eq!(
            finished,
            Event::TurnFinished {
                reason: StopReason::Completed,
                error: None
            }
        );
        running.stop().await;
    }

    #[tokio::test]
    async fn a_turn_outlives_its_client_and_the_next_client_is_caught_up() {
        let running = start(Arc::new(Hanging)).await;
        let client = connect(&running.socket, TERMINAL).await.unwrap();
        client.send(Request::SaveConnection(draft())).unwrap();
        client
            .send(Request::Prompt {
                text: "a long job".into(),
            })
            .unwrap();
        let (client, _) = until(client, |e| matches!(e, Event::UserMessage { .. })).await;
        // The connection drops in the middle of the turn.
        drop(client);

        let again = connect(&running.socket, TERMINAL).await.unwrap();
        let (again, state) = next(again).await;
        assert!(
            matches!(state, Some(Event::State(ref s)) if s.session.is_some()),
            "{state:?}"
        );
        let (again, history) = next(again).await;
        assert_eq!(
            history,
            Some(Event::History {
                events: vec![Event::UserMessage {
                    text: "a long job".into()
                }]
            })
        );

        // The turn is still running, and this client can stop it.
        again.send(Request::Cancel).unwrap();
        let (_, finished) = until(again, |e| matches!(e, Event::TurnFinished { .. })).await;
        assert_eq!(
            finished,
            Event::TurnFinished {
                reason: StopReason::Cancelled,
                error: None
            }
        );
        running.stop().await;
    }

    #[tokio::test]
    async fn a_socket_path_too_long_for_the_system_says_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("d".repeat(120)).join("tiphys.sock");
        let err = bind(&deep).unwrap_err().to_string();
        assert!(err.contains("set TIPHYS_SOCKET to a shorter path"), "{err}");
    }

    #[tokio::test]
    async fn each_audience_has_its_own_conversation() {
        let running = start(Arc::new(ReplayProvider::new(vec![says("One.")]))).await;
        let terminal = connect(&running.socket, TERMINAL).await.unwrap();
        terminal.send(Request::SaveConnection(draft())).unwrap();
        terminal
            .send(Request::Prompt {
                text: "from the app".into(),
            })
            .unwrap();
        let (_terminal, _) = until(terminal, |e| matches!(e, Event::TurnFinished { .. })).await;

        let oneshot = connect(&running.socket, ONESHOT).await.unwrap();
        let (oneshot, state) = next(oneshot).await;
        // It sees what is set up, but not the app's session or what was said in it.
        assert!(
            matches!(state, Some(Event::State(ref s)) if s.session.is_none() && s.connections.len() == 1)
        );
        let (_, history) = next(oneshot).await;
        assert_eq!(history, Some(Event::History { events: Vec::new() }));
        running.stop().await;
    }

    #[tokio::test]
    async fn telegram_is_asked_about_over_the_socket_and_answered_to_the_one_who_asked() {
        use tiphys_core::proto::{TelegramRequest, TelegramState};
        let running = start(Arc::new(ReplayProvider::new(vec![]))).await;
        let asking = connect(&running.socket, TERMINAL).await.unwrap();
        let other = connect(&running.socket, TERMINAL).await.unwrap();
        let (other, _) = until(other, |e| matches!(e, Event::History { .. })).await;

        asking
            .send(Request::Telegram(TelegramRequest::Status))
            .unwrap();
        let (asking, state) = until(asking, |e| matches!(e, Event::Telegram(_))).await;
        assert_eq!(state, Event::Telegram(TelegramState::default()));
        // Pairing without a bot is refused, and says what to do first.
        asking
            .send(Request::Telegram(TelegramRequest::Pair))
            .unwrap();
        let (_asking, failed) = until(asking, |e| matches!(e, Event::Failed { .. })).await;
        assert!(matches!(failed, Event::Failed { ref message } if message.contains("token first")));

        // The other client heard none of it.
        let quiet = tokio::task::spawn_blocking(move || {
            other.events.recv_timeout(Duration::from_millis(200))
        })
        .await
        .unwrap();
        assert!(quiet.is_err(), "{quiet:?}");
        running.stop().await;
    }

    #[tokio::test]
    async fn a_client_that_is_not_one_is_refused_with_the_reason() {
        let running = start(Arc::new(ReplayProvider::default())).await;
        let refusal = |result: Result<Client>| match result {
            Ok(_) => panic!("connected"),
            Err(e) => e.to_string(),
        };
        let unknown = refusal(connect(&running.socket, "telegram").await);
        assert!(
            unknown.contains("`telegram` is not something a client can talk as"),
            "{unknown}"
        );

        // A different protocol version.
        use tokio::io::AsyncReadExt;
        let mut raw = UnixStream::connect(&running.socket).await.unwrap();
        raw.write_all(b"{\"frame\":\"hello\",\"protocol\":999,\"version\":\"9.9.9\",\"audience\":\"terminal\"}\n").await.unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).await.unwrap();
        assert!(
            answer.contains("\"frame\":\"refused\"") && answer.contains("protocol 999"),
            "{answer}"
        );

        // Something that is not a hello at all.
        let mut raw = UnixStream::connect(&running.socket).await.unwrap();
        raw.write_all(b"GET / HTTP/1.1\n").await.unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).await.unwrap();
        assert!(answer.contains("has to be its hello"), "{answer}");

        // The daemon is unharmed.
        assert!(connect(&running.socket, TERMINAL).await.is_ok());
        running.stop().await;
    }

    #[tokio::test]
    async fn stopping_the_daemon_stops_a_running_turn_cleanly_and_closes_its_clients() {
        let running = start(Arc::new(Hanging)).await;
        let client = connect(&running.socket, TERMINAL).await.unwrap();
        client.send(Request::SaveConnection(draft())).unwrap();
        client
            .send(Request::Prompt {
                text: "a long job".into(),
            })
            .unwrap();
        let (client, _) = until(client, |e| matches!(e, Event::UserMessage { .. })).await;

        let dir = running.stop().await;
        // The client finds out by its connection closing.
        let mut client = client;
        loop {
            let (back, event) = next(client).await;
            client = back;
            if event.is_none() {
                break;
            }
        }
        // The turn was stopped, not cut off: its end is on the record.
        let sessions = tiphys_core::session::list(&dir.path().join("state")).unwrap();
        let history = tiphys_core::session::history(&dir.path().join("state"), &sessions[0].id);
        assert_eq!(
            history.last(),
            Some(&Event::TurnFinished {
                reason: StopReason::Cancelled,
                error: None
            })
        );

        // A new daemon on the same directory picks the session up.
        let running = start_in(dir, Arc::new(ReplayProvider::default())).await;
        let client = connect(&running.socket, TERMINAL).await.unwrap();
        let (_, state) = next(client).await;
        assert!(
            matches!(state, Some(Event::State(ref s)) if s.session.as_ref().is_some_and(|x| x.id == sessions[0].id))
        );
        running.stop().await;
    }
}
