//! Showing a turn in a chat.
//!
//! Each chat has one of these, fed the events of its host. It turns them
//! into messages:
//!
//! - what the agent says is sent as it is finished, split if it is long;
//! - what the agent does is one message per turn, a list that is edited as
//!   tools start and finish, and edited no more often than Telegram likes;
//! - a question is a message with two buttons, and is edited to show the
//!   answer once there is one.
//!
//! Text is sent plain. A model's Markdown shows as it was written.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiphys_core::proto::{Event, StopReason};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use super::api::{Api, pieces};

/// The least time between two edits of one message.
const EDIT_EVERY: Duration = Duration::from_millis(1500);
/// How long "typing…" shows after it is sent.
const TYPING_LASTS: Duration = Duration::from_secs(4);
/// How many of a turn's tool calls the activity message lists.
const ACTIVITY_LINES: usize = 12;
/// How much of a preview a question carries.
const PREVIEW_CHARS: usize = 1500;

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";

/// The questions that are waiting for a button, by the chat and message they
/// were asked in. The adapter looks a button press up here.
pub type Questions = Arc<Mutex<HashMap<(i64, i64), String>>>;

struct Line {
    id: String,
    summary: String,
    mark: &'static str,
}

pub struct Renderer {
    api: Arc<Api>,
    chat: i64,
    questions: Questions,
    /// The message listing what this turn has done, once there is one.
    activity: Option<i64>,
    lines: Vec<Line>,
    /// The list has changed since the message was last edited.
    stale: bool,
    edited: Instant,
    typed: Option<Instant>,
    /// What each open question said, to be shown again with its answer.
    asked: HashMap<String, (i64, String)>,
}

impl Renderer {
    pub fn new(api: Arc<Api>, chat: i64, questions: Questions) -> Self {
        Self {
            api,
            chat,
            questions,
            activity: None,
            lines: Vec::new(),
            stale: false,
            edited: Instant::now() - EDIT_EVERY,
            typed: None,
            asked: HashMap::new(),
        }
    }

    /// Shows events until the host stops sending them.
    pub async fn run(mut self, mut events: UnboundedReceiver<Event>) {
        loop {
            let due = self.edited + EDIT_EVERY;
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.show(event).await,
                    None => return,
                },
                () = tokio::time::sleep_until(due), if self.stale => self.flush().await,
            }
        }
    }

    async fn show(&mut self, event: Event) {
        match event {
            Event::UserMessage { .. } => {
                // A new turn has its own list of what was done.
                self.activity = None;
                self.lines.clear();
                self.typing().await;
            }
            Event::Text { .. } | Event::Reasoning { .. } => self.typing().await,
            Event::AssistantMessage { text } => {
                self.flush().await;
                self.say(&text).await;
            }
            Event::ToolStarted { id, summary, .. } => {
                self.lines.push(Line {
                    id,
                    summary,
                    mark: "…",
                });
                self.changed().await;
            }
            Event::ToolFinished { id, ok, .. } => {
                if let Some(line) = self.lines.iter_mut().rev().find(|line| line.id == id) {
                    line.mark = if ok { "✓" } else { "✗" };
                }
                self.changed().await;
            }
            Event::ApprovalRequested {
                id,
                summary,
                reason,
                why,
                preview,
                ..
            } => {
                let mut text = format!("Tiphys asks before it does this:\n{summary}");
                if !reason.is_empty() {
                    text.push_str(&format!("\n\nIts reason: {reason}"));
                }
                if !why.is_empty() {
                    text.push_str(&format!("\nIt asks because {why}."));
                }
                if let Some(preview) = preview {
                    let shown: String = preview.chars().take(PREVIEW_CHARS).collect();
                    let cut = if shown.len() < preview.len() {
                        "\n…"
                    } else {
                        ""
                    };
                    text.push_str(&format!("\n\n{shown}{cut}"));
                }
                let buttons = [("Approve", APPROVE), ("Deny", DENY)];
                if let Ok(message) = self.api.send_message(self.chat, &text, &buttons).await {
                    self.questions
                        .lock()
                        .unwrap()
                        .insert((self.chat, message), id.clone());
                    self.asked.insert(id, (message, text));
                }
            }
            Event::ApprovalResolved { id, approved, note } => {
                if let Some((message, text)) = self.asked.remove(&id) {
                    self.questions.lock().unwrap().remove(&(self.chat, message));
                    let answer = match (approved, note.is_empty()) {
                        (true, _) => "✓ Approved".to_string(),
                        (false, true) => "✗ Not approved".to_string(),
                        (false, false) => format!("✗ Not approved: {note}"),
                    };
                    // The buttons go, so that a question cannot be answered twice.
                    let _ = self
                        .api
                        .edit_message(self.chat, message, &format!("{text}\n\n{answer}"), &[])
                        .await;
                }
            }
            Event::Notice { text } => self.say(&text).await,
            Event::Failed { message } => self.say(&format!("✗ {message}")).await,
            Event::TurnFinished { reason, error } => {
                self.flush().await;
                match reason {
                    StopReason::Failed => {
                        self.say(&format!(
                            "✗ {}",
                            error.unwrap_or_else(|| "The turn failed.".into())
                        ))
                        .await;
                    }
                    StopReason::Cancelled => self.say("Stopped.").await,
                    StopReason::Stuck => {
                        self.say("Stopped: the model kept making the same call and getting the same result.").await;
                    }
                    StopReason::CutOff => {
                        self.say("Stopped: the reply kept hitting the output limit.")
                            .await
                    }
                    // These end with a message or a notice that says it all.
                    StopReason::Completed | StopReason::Rounds | StopReason::Budget => {}
                }
            }
            // What a host says to a client that attaches, and answers to
            // requests only the app makes.
            _ => {}
        }
    }

    async fn say(&mut self, text: &str) {
        for piece in pieces(text) {
            let _ = self.api.send_message(self.chat, &piece, &[]).await;
        }
    }

    async fn typing(&mut self) {
        if self.typed.is_none_or(|at| at.elapsed() >= TYPING_LASTS) {
            self.typed = Some(Instant::now());
            let _ = self.api.typing(self.chat).await;
        }
    }

    /// The list of what was done has changed: show it now, or soon if the
    /// message was edited a moment ago.
    async fn changed(&mut self) {
        self.stale = true;
        if self.edited.elapsed() >= EDIT_EVERY {
            self.flush().await;
        }
    }

    async fn flush(&mut self) {
        if !self.stale {
            return;
        }
        self.stale = false;
        self.edited = Instant::now();
        let hidden = self.lines.len().saturating_sub(ACTIVITY_LINES);
        let mut text: String = self.lines[hidden..]
            .iter()
            .map(|line| format!("· {} {}\n", line.summary, line.mark))
            .collect();
        if hidden > 0 {
            text = format!("… {hidden} earlier\n{text}");
        }
        let text = text.trim_end();
        match self.activity {
            Some(message) => {
                let _ = self.api.edit_message(self.chat, message, text, &[]).await;
            }
            None => self.activity = self.api.send_message(self.chat, text, &[]).await.ok(),
        }
    }
}
