//! Telegram: talking to the agent from a chat.
//!
//! The daemon asks Telegram for new messages and waits for the answer (long
//! polling), so the server opens no port and needs no address of its own.
//!
//! - **Who is answered.** A private chat with a user on the allowlist. Anyone
//!   else is not answered at all: no error, no hint that anything is here.
//!   Groups and channels are ignored.
//! - **Conversations.** Each chat is its own audience, with its own host and
//!   session. `/new` starts a fresh one; `/stop` stops what is running.
//! - **Late messages.** What the owner sent while the daemon was not running
//!   is not acted on when it starts: an instruction from hours ago may no
//!   longer be what the owner wants. The owner is told, and can send it again.
//! - **Questions.** An action that has to ask arrives as a message with two
//!   buttons. A press counts only from an allowed user, in the chat the
//!   question was asked in.
//! - **Pairing.** The owner never needs to know their Telegram id. The app
//!   shows a code; the owner sends it to the bot, and the app shows who it
//!   came from. Saying yes there puts that user on the allowlist. Someone
//!   who writes to the bot without the code is not shown and not answered.
//!
//! The bot's token is a key like any other: entered in the app, stored in
//! the key store, and sent only to Telegram.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tiphys_core::host::{ClientId, Input, Sink};
use tiphys_core::keys::{self, Secret};
use tiphys_core::proto::{Candidate, Event, Request, TelegramRequest, TelegramState};
use tiphys_core::{config, files, settings};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::hosts::Hosts;

pub mod api;
#[cfg(test)]
mod fake;
mod render;
#[cfg(test)]
mod tests;

use api::{Api, ApiError, Callback, Message, TELEGRAM, Update, User};
use render::{APPROVE, DENY, Questions, Renderer};

/// The name the bot's token is stored under.
pub const KEY: &str = keys::TELEGRAM;
/// Where the number of the next update is kept, so that a restart does not
/// read old messages again.
const OFFSET_FILE: &str = "telegram.offset";
/// How long one request for updates waits at Telegram.
const POLL_SECONDS: u64 = 50;
/// How long to wait after a request that did not get through before asking
/// again.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How long to wait after Telegram refused the token, or said something
/// else is reading the bot.
const RETRY_REFUSED_AFTER: Duration = Duration::from_secs(60);
/// How long the owner has to send the bot the code once pairing is opened.
const PAIR_WITHIN: Duration = Duration::from_secs(600);
/// How many messages that are not the code end pairing. A code has six
/// digits; nobody gets to try many of them.
const PAIR_MISSES: u32 = 20;

const LATE: &str = "This arrived while Tiphys was not running, so nothing was done about it. \
Send it again if it still stands.";

const GREETING: &str = "This is Tiphys. Send a message to begin.\n\n/new starts a fresh \
conversation\n/stop stops what is running";

/// Something the owner asked about Telegram from the app, and where the
/// answer goes.
pub struct Control {
    pub request: TelegramRequest,
    pub reply: Sink,
}

/// What Telegram answered.
enum Read {
    Bot(User),
    /// What was already waiting when the daemon started.
    Backlog(Vec<Update>),
    Updates(Vec<Update>),
}

/// A chat that has a host attached.
struct Link {
    inbox: UnboundedSender<Input>,
    client: ClientId,
}

struct Pairing {
    until: Instant,
    /// What the owner has to send the bot.
    code: String,
    /// Messages from strangers that were not the code.
    misses: u32,
    /// Who is told when the code arrives.
    watcher: Sink,
}

/// A code nobody can guess in the tries they get: six digits from the
/// random part of a fresh identifier.
fn new_code() -> String {
    format!("{:06}", uuid::Uuid::now_v7().as_u128() as u32 % 1_000_000)
}

pub struct Adapter {
    home: PathBuf,
    hosts: Arc<Hosts>,
    api: Option<Arc<Api>>,
    /// The bot's username, once Telegram has confirmed the token.
    bot: Option<String>,
    /// Why the bot is not being read, when it is not.
    problem: Option<String>,
    /// When to ask Telegram again, after it could not be asked.
    retry_at: Option<Instant>,
    /// Whether what was waiting at the start has been dealt with.
    caught_up: bool,
    /// The number of the next update to ask for.
    offset: i64,
    chats: HashMap<i64, Link>,
    questions: Questions,
    pairing: Option<Pairing>,
    candidate: Option<Candidate>,
}

impl Adapter {
    pub fn new(hosts: Arc<Hosts>) -> Self {
        let home = hosts.home().to_path_buf();
        let offset = std::fs::read_to_string(home.join(OFFSET_FILE))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        Self {
            home,
            hosts,
            api: None,
            bot: None,
            problem: None,
            retry_at: None,
            caught_up: false,
            offset,
            chats: HashMap::new(),
            questions: Questions::default(),
            pairing: None,
            candidate: None,
        }
    }

    /// Reads the bot's messages and the owner's requests until the daemon
    /// lets go of the way to ask.
    pub async fn run(mut self, mut control: UnboundedReceiver<Control>) {
        if let Ok(Some(token)) = keys::resolve(&self.home, KEY, None) {
            self.api = Api::new(&self.base(), token).ok().map(Arc::new);
        }
        loop {
            let (api, offset, wait) = (self.api.clone(), self.offset, self.retry_at);
            let (named, caught_up) = (self.bot.is_some(), self.caught_up);
            let read = async {
                let Some(api) = api else {
                    return std::future::pending().await;
                };
                if let Some(at) = wait {
                    tokio::time::sleep_until(at).await;
                }
                // The first thing asked of a token is whose it is. That is
                // also what tells a token Telegram does not know.
                if !named {
                    api.get_me().await.map(Read::Bot)
                } else if !caught_up {
                    // Asked without waiting: whatever comes back was sent
                    // before now.
                    api.get_updates(offset, 0).await.map(Read::Backlog)
                } else {
                    api.get_updates(offset, POLL_SECONDS)
                        .await
                        .map(Read::Updates)
                }
            };
            tokio::select! {
                asked = control.recv() => match asked {
                    Some(asked) => self.control(asked).await,
                    None => return,
                },
                read = read => self.read(read).await,
            }
        }
    }

    async fn read(&mut self, read: std::result::Result<Read, ApiError>) {
        match read {
            Ok(read) => {
                (self.problem, self.retry_at) = (None, None);
                match read {
                    Read::Bot(bot) => self.bot = Some(bot.username.unwrap_or(bot.first_name)),
                    Read::Backlog(updates) => {
                        self.caught_up = true;
                        self.backlog(updates).await;
                        self.keep_offset();
                    }
                    Read::Updates(updates) => {
                        for update in updates {
                            self.offset = self.offset.max(update.update_id + 1);
                            self.update(update).await;
                        }
                        // Kept only after the updates are dealt with. A crash
                        // in between repeats a message, which is better than
                        // losing one.
                        self.keep_offset();
                    }
                }
            }
            // The network, most likely. It is asked again soon, and nothing
            // is wrong with the bot as far as anyone can tell.
            Err(ApiError::Other(_) | ApiError::Slow(_)) => {
                self.retry_at = Some(Instant::now() + RETRY_AFTER);
            }
            // The token, or another reader. The owner is told, and it is
            // asked again now and then: both can pass without a restart.
            Err(e) => {
                self.problem = Some(e.to_string());
                self.retry_at = Some(Instant::now() + RETRY_REFUSED_AFTER);
            }
        }
    }

    /// Deals with what was waiting when the daemon started. The owner's
    /// messages are not acted on, and each chat is told so once. The rest
    /// is handled as ever: a stranger gets nothing, and a button belongs to
    /// a question that is no longer open.
    async fn backlog(&mut self, updates: Vec<Update>) {
        let allowed = self.allowed();
        let mut told = Vec::new();
        for update in updates {
            self.offset = self.offset.max(update.update_id + 1);
            let late = update.message.as_ref().and_then(|message| {
                let from = message.from.as_ref()?;
                (message.chat.kind == "private" && allowed.contains(&from.id))
                    .then_some(message.chat.id)
            });
            match late {
                Some(chat) if told.contains(&chat) => {}
                Some(chat) => {
                    told.push(chat);
                    if let Some(api) = &self.api {
                        let _ = api.send_message(chat, LATE, &[]).await;
                    }
                }
                None => self.update(update).await,
            }
        }
    }

    fn keep_offset(&self) {
        let _ = files::write_atomic(
            &self.home.join(OFFSET_FILE),
            self.offset.to_string().as_bytes(),
            files::SHARED_FILE,
        );
    }

    /// Where the Bot API is.
    fn base(&self) -> String {
        config::load_at(&self.home)
            .ok()
            .and_then(|config| config.telegram.api)
            .unwrap_or_else(|| TELEGRAM.to_string())
    }

    /// Checks a token with Telegram and starts reading with it. Says what is
    /// wrong if Telegram does not confirm it.
    async fn connect(&mut self, token: Secret) -> Option<String> {
        let checked = match Api::new(&self.base(), token) {
            Ok(api) => api.get_me().await.map(|bot| (api, bot)),
            Err(e) => Err(e),
        };
        match checked {
            Ok((api, bot)) => {
                self.api = Some(Arc::new(api));
                self.bot = Some(bot.username.unwrap_or(bot.first_name));
                (self.problem, self.retry_at) = (None, None);
                None
            }
            Err(e) => Some(e.to_string()),
        }
    }

    fn state(&self) -> TelegramState {
        TelegramState {
            bot: self.bot.clone(),
            allowed: self.allowed(),
            code: self
                .pairing
                .as_ref()
                .filter(|pairing| Instant::now() < pairing.until && self.candidate.is_none())
                .map(|pairing| pairing.code.clone()),
            candidate: self.candidate.clone(),
            problem: self.problem.clone(),
        }
    }

    /// The users who are answered. Read each time, so that a change in the
    /// configuration takes effect at once.
    fn allowed(&self) -> Vec<i64> {
        config::load_at(&self.home)
            .map(|config| config.telegram.allow)
            .unwrap_or_default()
    }

    async fn control(&mut self, asked: Control) {
        let Control { request, reply } = asked;
        let failed = |message: String| reply(&Event::Failed { message });
        match request {
            TelegramRequest::Status => {}
            TelegramRequest::SetToken { token } => {
                // The token in use stays in use until the new one has been
                // confirmed, so a mistyped one does not take the bot down.
                let was = (self.api.clone(), self.bot.clone());
                if let Some(problem) = self.connect(token.clone()).await {
                    return failed(problem);
                }
                if let Err(e) = keys::store(&self.home, KEY, &token) {
                    (self.api, self.bot) = was;
                    return failed(e.to_string());
                }
                // A different bot has different chats and different updates.
                self.forget_chats();
                self.offset = 0;
                self.caught_up = false;
                self.keep_offset();
                (self.pairing, self.candidate) = (None, None);
            }
            TelegramRequest::Pair => {
                if self.api.is_none() {
                    return failed("there is no bot yet; enter its token first".into());
                }
                self.candidate = None;
                self.pairing = Some(Pairing {
                    until: Instant::now() + PAIR_WITHIN,
                    code: new_code(),
                    misses: 0,
                    watcher: reply.clone(),
                });
            }
            TelegramRequest::Allow { user } => {
                // Only who pairing found: what the owner said yes to in the
                // app is the user who sent the code.
                if self.candidate.as_ref().map(|found| found.id) != Some(user) {
                    return failed("that is not who sent the code; pair again".into());
                }
                if self
                    .pairing
                    .as_ref()
                    .is_none_or(|pairing| Instant::now() >= pairing.until)
                {
                    (self.pairing, self.candidate) = (None, None);
                    return failed("pairing ran out before it was answered; pair again".into());
                }
                if let Err(e) = settings::allow_telegram(&self.home, user) {
                    return failed(e.to_string());
                }
                self.pairing = None;
                self.candidate = None;
                if let Some(api) = &self.api {
                    let _ = api
                        .send_message(
                            user,
                            &format!("You can talk to Tiphys here now.\n\n{GREETING}"),
                            &[],
                        )
                        .await;
                }
            }
            TelegramRequest::Remove => {
                if let Err(e) = keys::remove(&self.home, KEY) {
                    return failed(e.to_string());
                }
                self.forget_chats();
                (self.api, self.bot, self.problem, self.retry_at) = (None, None, None, None);
                (self.pairing, self.candidate) = (None, None);
                self.offset = 0;
                self.caught_up = false;
                self.keep_offset();
            }
        }
        reply(&Event::Telegram(self.state()));
    }

    fn forget_chats(&mut self) {
        for (_, link) in self.chats.drain() {
            let _ = link.inbox.send(Input::Detach {
                client: link.client,
            });
        }
        self.questions.lock().unwrap().clear();
    }

    async fn update(&mut self, update: Update) {
        if let Some(message) = update.message {
            self.message(message).await;
        } else if let Some(callback) = update.callback_query {
            self.pressed(callback).await;
        }
    }

    async fn message(&mut self, message: Message) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let Some(from) = message.from.filter(|from| !from.is_bot) else {
            return;
        };
        // One person, one chat. A group is a room full of people who are not
        // the owner.
        if message.chat.kind != "private" {
            return;
        }
        let chat = message.chat.id;
        if !self.allowed().contains(&from.id) {
            self.stranger(from, message.text.as_deref().unwrap_or_default());
            return;
        }
        let Some(text) = message
            .text
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            let _ = api
                .send_message(chat, "Tiphys reads text only for now.", &[])
                .await;
            return;
        };
        let request = match text.as_str() {
            "/start" | "/help" => {
                let _ = api.send_message(chat, GREETING, &[]).await;
                return;
            }
            "/new" => {
                let _ = api
                    .send_message(
                        chat,
                        "A new conversation starts with your next message.",
                        &[],
                    )
                    .await;
                Request::NewSession
            }
            "/stop" => Request::Cancel,
            other if other.starts_with('/') => {
                let _ = api
                    .send_message(chat, "Tiphys knows /new and /stop.", &[])
                    .await;
                return;
            }
            _ => Request::Prompt { text },
        };
        self.send(chat, &api, request);
    }

    /// Someone who is not allowed wrote to the bot. They get nothing. If the
    /// owner has pairing open and this is the code, the owner is shown who
    /// sent it.
    fn stranger(&mut self, from: User, text: &str) {
        let Some(pairing) = &mut self.pairing else {
            return;
        };
        if Instant::now() >= pairing.until {
            self.pairing = None;
            return;
        }
        // The first to send the code is the one; nobody replaces them.
        if self.candidate.is_some() {
            return;
        }
        let watcher = pairing.watcher.clone();
        if text.trim() == pairing.code {
            self.candidate = Some(Candidate {
                id: from.id,
                name: from.first_name,
                username: from.username,
            });
        } else {
            pairing.misses += 1;
            if pairing.misses < PAIR_MISSES {
                return;
            }
            self.pairing = None;
        }
        watcher(&Event::Telegram(self.state()));
    }

    async fn pressed(&mut self, callback: Callback) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let answer = |text: &'static str| {
            let (api, id) = (api.clone(), callback.id.clone());
            async move {
                let _ = api.answer_callback(&id, text).await;
            }
        };
        if !self.allowed().contains(&callback.from.id) {
            return;
        }
        let Some(message) = &callback.message else {
            return answer("This question is no longer open.").await;
        };
        let chat = message.chat.id;
        let question = self
            .questions
            .lock()
            .unwrap()
            .get(&(chat, message.message_id))
            .cloned();
        let (Some(id), Some(data)) = (question, callback.data.as_deref()) else {
            return answer("This question is no longer open.").await;
        };
        let approve = match data {
            APPROVE => true,
            DENY => false,
            _ => return answer("").await,
        };
        self.send(
            chat,
            &api,
            Request::Approval {
                id,
                approve,
                note: None,
            },
        );
        answer("").await;
    }

    /// Sends a request to a chat's host, attaching the chat to it first if
    /// this is the first thing the chat has said since the daemon started.
    fn send(&mut self, chat: i64, api: &Arc<Api>, request: Request) {
        let link = self.chats.entry(chat).or_insert_with(|| {
            let inbox = self.hosts.inbox(&format!("telegram:{chat}"));
            let client = self.hosts.client_id();
            let (events, shown) = unbounded_channel();
            let sink: Sink = Arc::new(move |event| {
                let _ = events.send(event.clone());
            });
            let _ = inbox.send(Input::Attach { client, sink });
            tokio::spawn(Renderer::new(api.clone(), chat, self.questions.clone()).run(shown));
            Link { inbox, client }
        });
        let _ = link.inbox.send(Input::Request {
            client: link.client,
            request,
        });
    }
}
