use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tiphys_core::config::Connection;
use tiphys_core::keys::{self, Secret};
use tiphys_core::llm::{Connect, Delta, Provider, ReplayProvider, ToolCallPart};
use tiphys_core::proto::{Event, StopReason, TelegramRequest, TelegramState};
use tiphys_core::{Result, session, settings};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::fake::{Fake, TOKEN};
use super::render::{Questions, Renderer};
use super::*;

const OWNER: i64 = 42;
const STRANGER: i64 = 99;

struct Scripted(Arc<dyn Provider>);

impl Connect for Scripted {
    fn provider(&self, _: &Connection, _: Option<&Secret>) -> Result<Arc<dyn Provider>> {
        Ok(self.0.clone())
    }
}

/// A provider whose reply starts and never ends.
struct Hanging;

#[async_trait::async_trait]
impl Provider for Hanging {
    async fn stream(&self, _: tiphys_core::llm::Request) -> Result<tiphys_core::llm::DeltaStream> {
        use futures_util::StreamExt;
        let start = futures_util::stream::iter(vec![Ok(Delta::Text("thinking".into()))]);
        Ok(Box::pin(start.chain(futures_util::stream::pending())))
    }
    async fn models(&self) -> Result<Vec<tiphys_core::llm::Model>> {
        Ok(Vec::new())
    }
}

fn says(text: &str) -> Vec<Delta> {
    vec![Delta::Text(text.into()), Delta::Done]
}

fn writes(id: &str, path: &str) -> Vec<Delta> {
    let arguments = json!({"path": path, "content": "written\n", "reason": "to keep a note"});
    let call = ToolCallPart {
        slot: Some(0),
        id: id.into(),
        name: "write_file".into(),
        arguments: arguments.to_string(),
    };
    vec![Delta::ToolCall(call), Delta::Done]
}

/// What the app would be sent.
#[derive(Clone, Default)]
struct App(Arc<Mutex<Vec<Event>>>);

impl App {
    fn sink(&self) -> Sink {
        let seen = self.0.clone();
        Arc::new(move |event| seen.lock().unwrap().push(event.clone()))
    }

    fn last(&self) -> Event {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the app was sent nothing")
    }

    fn state(&self) -> TelegramState {
        match self.last() {
            Event::Telegram(state) => state,
            other => panic!("not where Telegram stands: {other:?}"),
        }
    }
}

struct Running {
    dir: tempfile::TempDir,
    fake: Fake,
    hosts: Arc<Hosts>,
    control: UnboundedSender<Control>,
    adapter: tokio::task::JoinHandle<()>,
}

impl Running {
    fn home(&self) -> std::path::PathBuf {
        self.dir.path().join("state")
    }

    /// Asks as the app does, and waits for the answer.
    async fn ask(&self, request: TelegramRequest) -> Event {
        let app = App::default();
        self.ask_as(&app, request).await
    }

    async fn ask_as(&self, app: &App, request: TelegramRequest) -> Event {
        let before = app.0.lock().unwrap().len();
        self.control
            .send(Control {
                request,
                reply: app.sink(),
            })
            .ok()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while app.0.lock().unwrap().len() == before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the adapter did not answer");
        app.last()
    }

    /// Stops the adapter and its hosts, as stopping the daemon would.
    async fn stopped(self) -> (tempfile::TempDir, Fake) {
        drop(self.control);
        self.adapter.await.unwrap();
        self.hosts.stop().await;
        (self.dir, self.fake)
    }

    /// Stops, and starts again on the same state, as a restart would.
    async fn restart(self, provider: Arc<dyn Provider>) -> Self {
        let (dir, fake) = self.stopped().await;
        let before = fake.long_polls();
        let running = begin(dir, fake, provider);
        running.fake.listening(before).await;
        running
    }
}

fn begin(dir: tempfile::TempDir, fake: Fake, provider: Arc<dyn Provider>) -> Running {
    let hosts = Arc::new(Hosts::new(
        &dir.path().join("state"),
        &dir.path().join("user"),
        Arc::new(Scripted(provider)),
    ));
    let (control, asked) = unbounded_channel();
    let adapter = tokio::spawn(Adapter::new(hosts.clone()).run(asked));
    Running {
        dir,
        fake,
        hosts,
        control,
        adapter,
    }
}

/// A Tiphys with a model connection, pointed at a fake Telegram. `config`
/// is the rest of the owner's configuration.
async fn start_with(provider: Arc<dyn Provider>, config: &str, token: Option<&str>) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::start().await;
    let home = dir.path().join("state");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(dir.path().join("user")).unwrap();
    let connection = Connection {
        base_url: "https://api.example.com/v1".into(),
        model: Some("m".into()),
        env_key: None,
        local: true,
    };
    settings::save_connection(&home, "work", &connection).unwrap();
    settings::set_default_connection(&home, Some("work")).unwrap();
    std::fs::write(
        home.join("config.toml"),
        config.replace("{api}", &fake.base),
    )
    .unwrap();
    if let Some(token) = token {
        keys::store(&home, KEY, &Secret::new(token).unwrap()).unwrap();
    }
    let running = begin(dir, fake, provider);
    // Past what was waiting for it, so that what a test sends is new.
    if token == Some(TOKEN) {
        running.fake.listening(0).await;
    }
    running
}

/// The usual: the bot's token is stored and the owner is allowed.
async fn start(replies: Vec<Vec<Delta>>) -> Running {
    let config = format!("[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n");
    start_with(Arc::new(ReplayProvider::new(replies)), &config, Some(TOKEN)).await
}

fn sessions(running: &Running) -> Vec<String> {
    session::list(&running.home())
        .unwrap()
        .into_iter()
        .map(|meta| meta.audience)
        .collect()
}

#[tokio::test]
async fn a_stranger_gets_nothing_at_all() {
    let running = start(vec![says("Hello.")]).await;
    running.fake.say(STRANGER, "hello?");
    running.fake.say(STRANGER, "/start");
    running.fake.press(STRANGER, 1000, APPROVE);
    running.fake.settled().await;

    // Not a message, not "typing…", not an answer to the button.
    for method in [
        "sendMessage",
        "sendChatAction",
        "answerCallbackQuery",
        "editMessageText",
    ] {
        assert_eq!(
            running.fake.calls(method),
            Vec::<serde_json::Value>::new(),
            "{method}"
        );
    }
    assert!(sessions(&running).is_empty());
}

#[tokio::test]
async fn the_owner_gets_an_answer_in_a_conversation_of_their_chat() {
    let running = start(vec![says("Hello from the server.")]).await;
    running.fake.say(OWNER, "are you there?");
    running
        .fake
        .wait_said(OWNER, "Hello from the server.")
        .await;

    assert_eq!(running.fake.said(OWNER), ["Hello from the server."]);
    assert!(!running.fake.calls("sendChatAction").is_empty());
    assert_eq!(sessions(&running), [format!("telegram:{OWNER}")]);
}

#[tokio::test]
async fn a_group_is_not_answered_even_when_the_owner_writes_in_it() {
    let running = start(vec![says("Hello.")]).await;
    running.fake.say_in(OWNER, -5000, "group", "are you there?");
    running.fake.say_in(OWNER, -5001, "supergroup", "/new");
    running.fake.settled().await;
    assert!(running.fake.calls("sendMessage").is_empty());
    assert!(sessions(&running).is_empty());
}

#[tokio::test]
async fn what_has_to_ask_arrives_with_buttons_and_only_the_owners_press_counts() {
    let provider = Arc::new(ReplayProvider::new(vec![
        writes("w1", "notes.txt"),
        says("Saved."),
    ]));
    let config = format!(
        "[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n\n[approvals]\nchange = \"ask\"\n"
    );
    let running = start_with(provider, &config, Some(TOKEN)).await;
    let note = running.dir.path().join("user/notes.txt");

    running.fake.say(OWNER, "keep a note");
    let asked = running
        .fake
        .wait("sendMessage", |body| body.get("reply_markup").is_some())
        .await;
    let text = asked["text"].as_str().unwrap();
    assert!(
        text.contains("notes.txt") && text.contains("to keep a note"),
        "{text}"
    );
    assert!(text.contains("+written"), "{text}");
    assert_eq!(
        asked["reply_markup"]["inline_keyboard"],
        json!([[{"text": "Approve", "callback_data": "approve"}, {"text": "Deny", "callback_data": "deny"}]])
    );
    let question = asked["sent_as"].as_i64().unwrap();

    // Someone else presses it, and the owner presses a button that is not there.
    running.fake.press(STRANGER, question, APPROVE);
    running.fake.press(OWNER, question + 500, APPROVE);
    running.fake.settled().await;
    assert!(!note.exists());

    running.fake.press(OWNER, question, APPROVE);
    running.fake.wait_said(OWNER, "Saved.").await;
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "written\n");
    // The question now shows its answer and has no buttons left.
    let edited = running
        .fake
        .wait("editMessageText", |body| body["message_id"] == question)
        .await;
    assert!(
        edited["text"].as_str().unwrap().ends_with("✓ Approved"),
        "{edited}"
    );
    assert_eq!(edited["reply_markup"], json!({"inline_keyboard": []}));
    // A second press finds no open question.
    running.fake.press(OWNER, question, DENY);
    running
        .fake
        .wait("answerCallbackQuery", |body| {
            body["text"] == "This question is no longer open."
        })
        .await;
}

#[tokio::test]
async fn a_denied_question_is_not_done_and_says_so() {
    let provider = Arc::new(ReplayProvider::new(vec![
        writes("w1", "notes.txt"),
        says("Left alone."),
    ]));
    let config = format!(
        "[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n\n[approvals]\nchange = \"ask\"\n"
    );
    let running = start_with(provider, &config, Some(TOKEN)).await;

    running.fake.say(OWNER, "keep a note");
    let asked = running
        .fake
        .wait("sendMessage", |body| body.get("reply_markup").is_some())
        .await;
    running
        .fake
        .press(OWNER, asked["sent_as"].as_i64().unwrap(), DENY);
    running.fake.wait_said(OWNER, "Left alone.").await;

    assert!(!running.dir.path().join("user/notes.txt").exists());
    let edited = running
        .fake
        .wait("editMessageText", |body| {
            body["message_id"] == asked["sent_as"]
        })
        .await;
    assert!(
        edited["text"].as_str().unwrap().contains("✗ Not approved"),
        "{edited}"
    );
}

#[tokio::test]
async fn stop_ends_what_is_running() {
    let config = format!("[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n");
    let running = start_with(Arc::new(Hanging), &config, Some(TOKEN)).await;
    running.fake.say(OWNER, "think about it");
    running.fake.wait("sendChatAction", |_| true).await;
    running.fake.say(OWNER, "/stop");
    running.fake.wait_said(OWNER, "Stopped.").await;
}

#[tokio::test]
async fn new_starts_a_fresh_conversation_and_a_message_sent_meanwhile_waits_its_turn() {
    let provider = Arc::new(ReplayProvider::new(vec![
        says("First."),
        says("Second."),
        says("Third."),
    ]));
    let config = format!("[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n");
    let running = start_with(provider.clone(), &config, Some(TOKEN)).await;

    // Two messages at once: the second is answered after the first.
    running.fake.say(OWNER, "one");
    running.fake.say(OWNER, "two");
    running.fake.wait_said(OWNER, "Second.").await;
    assert_eq!(running.fake.said(OWNER), ["First.", "Second."]);
    assert_eq!(provider.requests()[1].messages.len(), 3);

    running.fake.say(OWNER, "/new");
    running.fake.say(OWNER, "three");
    running.fake.wait_said(OWNER, "Third.").await;
    // The model was sent the new conversation alone.
    assert_eq!(provider.requests()[2].messages.len(), 1);
    assert_eq!(sessions(&running).len(), 2);

    running.fake.say(OWNER, "/reboot");
    running
        .fake
        .wait_said(OWNER, "Tiphys knows /new and /stop.")
        .await;
}

#[tokio::test]
async fn the_owner_is_paired_by_sending_the_bot_a_code_from_the_app() {
    // No token yet, and nobody is allowed.
    let provider = Arc::new(ReplayProvider::new(vec![says("Hello, owner.")]));
    let running = start_with(provider, "[telegram]\napi = \"{api}\"\n", None).await;
    let app = App::default();
    assert_eq!(
        running.ask_as(&app, TelegramRequest::Status).await,
        Event::Telegram(TelegramState::default())
    );
    let failed = running.ask_as(&app, TelegramRequest::Pair).await;
    assert!(
        matches!(failed, Event::Failed { ref message } if message.contains("token first")),
        "{failed:?}"
    );

    // The token is checked with Telegram, then kept as a key.
    let token = Secret::new(TOKEN).unwrap();
    running
        .ask_as(&app, TelegramRequest::SetToken { token })
        .await;
    assert_eq!(app.state().bot.as_deref(), Some("tiphys_bot"));
    assert!(keys::is_stored(&running.home(), KEY));
    running.fake.listening(0).await;

    running.ask_as(&app, TelegramRequest::Pair).await;
    let code = app.state().code.expect("pairing shows a code");
    assert_eq!(code.len(), 6);

    // Someone who does not have the code is not shown, and not answered.
    running.fake.say(STRANGER, "let me in");
    running.fake.say(STRANGER, "000000x");
    running.fake.settled().await;
    assert_eq!(app.state().candidate, None);

    // The owner sends it, and the app is told who that was.
    running.fake.say(OWNER, &format!(" {code}\n"));
    running.fake.settled().await;
    let found = app
        .state()
        .candidate
        .expect("the app is shown who sent the code");
    assert_eq!(
        (found.id, found.name.as_str(), found.username.as_deref()),
        (OWNER, "User42", Some("user42"))
    );
    // Once someone has sent it, nobody takes their place.
    running.fake.say(STRANGER, &code);
    running.fake.settled().await;
    assert_eq!(app.state().candidate.map(|found| found.id), Some(OWNER));
    assert!(running.fake.calls("sendMessage").is_empty());

    // Only who was found can be allowed.
    let refused = running
        .ask_as(&app, TelegramRequest::Allow { user: STRANGER })
        .await;
    assert!(matches!(refused, Event::Failed { .. }), "{refused:?}");
    running
        .ask_as(&app, TelegramRequest::Allow { user: OWNER })
        .await;
    let state = app.state();
    assert_eq!(
        (state.allowed, state.code, state.candidate),
        (vec![OWNER], None, None)
    );
    running
        .fake
        .wait_said(OWNER, "You can talk to Tiphys here now.")
        .await;

    running.fake.say(OWNER, "hello");
    running.fake.wait_said(OWNER, "Hello, owner.").await;
    running.fake.say(STRANGER, "and me?");
    running.fake.settled().await;
    assert!(running.fake.said(STRANGER).is_empty());
}

#[tokio::test]
async fn pairing_ends_after_too_many_wrong_codes() {
    let running = start_with(
        Arc::new(ReplayProvider::new(vec![])),
        "[telegram]\napi = \"{api}\"\n",
        Some(TOKEN),
    )
    .await;
    let app = App::default();
    running.ask_as(&app, TelegramRequest::Pair).await;
    let code = app.state().code.unwrap();
    for guess in 0..PAIR_MISSES {
        running.fake.say(STRANGER, &format!("guess {guess}"));
    }
    running.fake.settled().await;
    assert_eq!(app.state().code, None);

    running.fake.say(STRANGER, &code);
    running.fake.settled().await;
    assert_eq!(app.state().candidate, None);
}

#[tokio::test]
async fn a_token_telegram_does_not_know_is_refused_and_the_one_in_use_stays() {
    let running = start(vec![says("Still here.")]).await;
    let wrong = Secret::new("999:not-a-token").unwrap();
    let failed = running
        .ask(TelegramRequest::SetToken { token: wrong })
        .await;
    assert!(
        matches!(failed, Event::Failed { ref message } if message == "Telegram does not accept this bot token"),
        "{failed:?}"
    );
    // The token never shows in what the app is told.
    assert!(!format!("{failed:?}").contains("not-a-token"));
    assert_eq!(
        keys::resolve(&running.home(), KEY, None)
            .unwrap()
            .unwrap()
            .expose(),
        TOKEN
    );

    running.fake.say(OWNER, "hello");
    running.fake.wait_said(OWNER, "Still here.").await;
}

#[tokio::test]
async fn a_stored_token_telegram_refuses_is_a_problem_the_app_is_told() {
    let config = format!("[telegram]\napi = \"{{api}}\"\nallow = [{OWNER}]\n");
    let running = start_with(
        Arc::new(ReplayProvider::new(vec![])),
        &config,
        Some("999:revoked"),
    )
    .await;
    // It is found out when the daemon first asks Telegram whose token it is.
    let state = loop {
        match running.ask(TelegramRequest::Status).await {
            Event::Telegram(state) if state.problem.is_some() => break state,
            _ => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    };
    assert_eq!(state.bot, None);
    assert_eq!(
        state.problem.as_deref(),
        Some("Telegram does not accept this bot token")
    );
}

#[tokio::test]
async fn removing_the_bot_forgets_its_token_and_stops_reading() {
    let running = start(vec![says("Hello.")]).await;
    let Event::Telegram(state) = running.ask(TelegramRequest::Remove).await else {
        panic!("not where Telegram stands");
    };
    assert_eq!((state.bot, state.problem), (None, None));
    assert!(!keys::is_stored(&running.home(), KEY));

    let before = running.fake.asked_from();
    running.fake.say(OWNER, "hello?");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(running.fake.asked_from(), before);
    assert!(running.fake.calls("sendMessage").is_empty());
}

#[tokio::test]
async fn a_restart_does_not_read_old_messages_again_and_picks_the_chat_back_up() {
    let running = start(vec![says("First.")]).await;
    let first = running.fake.say(OWNER, "one");
    running.fake.wait_said(OWNER, "First.").await;
    running.fake.settled().await;
    assert_eq!(
        std::fs::read_to_string(running.home().join(OFFSET_FILE)).unwrap(),
        (first + 1).to_string()
    );

    let provider = Arc::new(ReplayProvider::new(vec![says("Second.")]));
    let running = running.restart(provider.clone()).await;
    running.fake.say(OWNER, "two");
    running.fake.wait_said(OWNER, "Second.").await;
    // "one" was not answered twice, and the conversation went on where it was.
    assert_eq!(running.fake.said(OWNER), ["First.", "Second."]);
    assert_eq!(provider.requests()[0].messages.len(), 3);
    assert_eq!(sessions(&running).len(), 1);
}

#[tokio::test]
async fn what_was_sent_while_tiphys_was_not_running_is_not_acted_on_and_the_owner_is_told() {
    let (dir, fake) = start(vec![]).await.stopped().await;
    fake.say(OWNER, "restart the web server");
    fake.say(OWNER, "/new");
    fake.say(STRANGER, "anyone there?");
    let late = fake.press(OWNER, 1000, APPROVE);

    let provider = Arc::new(ReplayProvider::new(vec![says("Done now.")]));
    let before = fake.long_polls();
    let running = begin(dir, fake, provider.clone());
    running.fake.listening(before).await;
    // Told once, however many messages were waiting; nothing was run.
    assert_eq!(running.fake.said(OWNER), [LATE]);
    assert!(running.fake.said(STRANGER).is_empty());
    assert!(provider.requests().is_empty() && sessions(&running).is_empty());
    assert_eq!(running.fake.asked_from(), late + 1);

    // What is sent from now on is acted on as ever.
    running.fake.say(OWNER, "restart the web server");
    running.fake.wait_said(OWNER, "Done now.").await;
}

/// A renderer for one chat, fed by hand.
async fn rendered(events: Vec<Event>) -> Fake {
    let fake = Fake::start().await;
    let api = Arc::new(Api::new(&fake.base, Secret::new(TOKEN).unwrap()).unwrap());
    let (feed, shown) = unbounded_channel();
    for event in events {
        feed.send(event).unwrap();
    }
    drop(feed);
    Renderer::new(api, OWNER, Questions::default())
        .run(shown)
        .await;
    fake
}

#[tokio::test]
async fn what_a_turn_does_is_one_message_that_is_kept_up_to_date() {
    let started = |id: &str, summary: &str| Event::ToolStarted {
        id: id.into(),
        tool: "shell".into(),
        summary: summary.into(),
        reason: String::new(),
    };
    let finished = |id: &str, ok| Event::ToolFinished {
        id: id.into(),
        ok,
        output: String::new(),
    };
    let fake = rendered(vec![
        Event::UserMessage {
            text: "check the disk".into(),
        },
        started("a", "run `df -h`"),
        finished("a", true),
        started("b", "read /etc/fstab"),
        finished("b", false),
        Event::AssistantMessage {
            text: "The disk is fine.".into(),
        },
        Event::TurnFinished {
            reason: StopReason::Completed,
            error: None,
        },
    ])
    .await;

    // One message for what was done, edited to its final state before the answer.
    assert_eq!(fake.said(OWNER), ["· run `df -h` …", "The disk is fine."]);
    let edits = fake.calls("editMessageText");
    assert_eq!(
        edits.last().unwrap()["text"],
        "· run `df -h` ✓\n· read /etc/fstab ✗"
    );
    assert!(edits.len() <= 2, "{edits:?}");
}

#[tokio::test]
async fn a_question_nobody_answers_shows_that_it_was_not_approved_and_why() {
    let fake = rendered(vec![
        Event::ApprovalRequested {
            id: "q".into(),
            tool: "shell".into(),
            summary: "run `sudo apt upgrade`".into(),
            reason: "to bring the system up to date".into(),
            why: "it runs as root".into(),
            class: tiphys_core::policy::Class::System,
            preview: None,
        },
        Event::ApprovalResolved {
            id: "q".into(),
            approved: false,
            note: "nobody answered within 5 minutes".into(),
        },
        Event::TurnFinished {
            reason: StopReason::Failed,
            error: Some("the provider said no".into()),
        },
    ])
    .await;
    let asked = &fake.calls("sendMessage")[0];
    assert_eq!(
        asked["text"],
        "Tiphys asks before it does this:\nrun `sudo apt upgrade`\n\nIts reason: to bring the system up to date\nIt asks because it runs as root."
    );
    let edited = &fake.calls("editMessageText")[0];
    assert!(
        edited["text"]
            .as_str()
            .unwrap()
            .ends_with("✗ Not approved: nobody answered within 5 minutes")
    );
    assert_eq!(fake.said(OWNER).last().unwrap(), "✗ the provider said no");
}

#[tokio::test]
async fn a_long_answer_is_sent_in_pieces() {
    let long: String = (0..400)
        .map(|n| format!("line number {n} of a long answer\n"))
        .collect();
    let fake = rendered(vec![Event::AssistantMessage { text: long.clone() }]).await;
    let said = fake.said(OWNER);
    assert!(said.len() > 1);
    assert_eq!(said.concat(), long);
}
