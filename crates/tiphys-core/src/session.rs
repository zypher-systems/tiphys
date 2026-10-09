//! A session: one conversation and everything said in it.
//!
//! ```text
//! sessions/<id>/
//!   meta.json         who it is with, which model, when it began
//!   system.md         the system prompt, fixed when the session opened
//!   tools.json        the tools the model was told about, fixed with it
//!   transcript.jsonl  the messages, in order
//!   events.jsonl      what happened, numbered, for a client to replay
//! ```
//!
//! The prompt and the tool list are written once and reused byte for byte for
//! as long as the session lives, so the provider's cache of the conversation's
//! opening holds across turns and restarts. The transcript and the events are
//! only ever appended to.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::compact::{self, View};
use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir, write_atomic};
use crate::llm::{Message, Role, ToolSpec};
use crate::proto::{Event, StopReason};
use crate::{Error, Result, jsonl};

/// The directory under the state directory that holds the sessions.
pub const SESSIONS_DIR: &str = "sessions";

const META: &str = "meta.json";
const SYSTEM: &str = "system.md";
const TOOLS: &str = "tools.json";
const TRANSCRIPT: &str = "transcript.jsonl";
const EVENTS: &str = "events.jsonl";
/// How many events of a session a client is sent when it attaches.
const HISTORY_EVENTS: usize = 400;
/// What a turn is closed with when Tiphys stopped while it was running.
pub const INTERRUPTED: &str = "Tiphys stopped while this turn was running";

/// What is known about a session without reading its transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub created: DateTime<Utc>,
    pub connection: String,
    pub model: String,
    /// Who the session is with: the terminal, or later one chat. Memory will
    /// be kept apart by this.
    pub audience: String,
    /// The start of the first message, for lists.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// What a new session is made from.
#[derive(Debug, Clone)]
pub struct Opening {
    pub connection: String,
    pub model: String,
    pub audience: String,
    pub system: String,
    pub tools: Vec<ToolSpec>,
}

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Message {
        at: DateTime<Utc>,
        message: Message,
    },
    /// From here on the model is sent less than the whole transcript: it
    /// starts at message `start`, and tool results before `trim_before` are
    /// left out. Every message is still in the file.
    Compacted {
        at: DateTime<Utc>,
        start: usize,
        trim_before: usize,
    },
}

/// One line of the event file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub event: Event,
}

/// An open session.
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    meta: Meta,
    system: String,
    tools: Vec<ToolSpec>,
    messages: Vec<Message>,
    /// How much of the transcript the model is sent.
    view: View,
    seq: u64,
}

impl Session {
    /// Starts a session and writes its fixed files.
    pub fn create(home: &Path, opening: Opening) -> Result<Self> {
        let meta = Meta {
            id: uuid::Uuid::now_v7().to_string(),
            created: Utc::now(),
            connection: opening.connection,
            model: opening.model,
            audience: opening.audience,
            title: String::new(),
        };
        let dir = home.join(SESSIONS_DIR).join(&meta.id);
        ensure_dir(&dir, SHARED_DIR)?;
        let tools = serde_json::to_vec_pretty(&opening.tools)
            .map_err(|e| Error::Io(format!("cannot encode the tool list: {e}")))?;
        write_atomic(&dir.join(SYSTEM), opening.system.as_bytes(), SHARED_FILE)?;
        write_atomic(&dir.join(TOOLS), &tools, SHARED_FILE)?;
        let session = Self {
            dir,
            meta,
            system: opening.system,
            tools: opening.tools,
            messages: Vec::new(),
            view: View::default(),
            seq: 0,
        };
        // Written last: a directory without it is a session that never began.
        session.write_meta()?;
        Ok(session)
    }

    /// Opens an existing session with its history.
    pub fn open(home: &Path, id: &str) -> Result<Self> {
        // An id is only ever hex and dashes; anything else is not a session
        // and must not become a path.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(Error::Config(format!("`{id}` is not a session id")));
        }
        let dir = home.join(SESSIONS_DIR).join(id);
        let meta =
            read_meta(&dir)?.ok_or_else(|| Error::Config(format!("there is no session `{id}`")))?;
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .map_err(|e| Error::Io(format!("{}: {e}", dir.join(name).display())))
        };
        let system = read(SYSTEM)?;
        let tools = serde_json::from_str(&read(TOOLS)?)
            .map_err(|e| Error::Io(format!("{}: {e}", dir.join(TOOLS).display())))?;
        let mut messages = Vec::new();
        let mut view = View::default();
        for record in jsonl::read::<Record>(&dir.join(TRANSCRIPT))? {
            match record {
                Record::Message { message, .. } => messages.push(message),
                Record::Compacted {
                    start, trim_before, ..
                } => view = View { start, trim_before },
            }
        }
        let seq = jsonl::read::<Stamped>(&dir.join(EVENTS))?
            .last()
            .map_or(0, |stamped| stamped.seq);
        Ok(Self {
            dir,
            meta,
            system,
            tools,
            messages,
            view,
            seq,
        })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The system prompt, as it was when the session opened.
    pub fn system(&self) -> &str {
        &self.system
    }

    /// The tools the model was told about when the session opened.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// Every message of the session, from the first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The messages the model is sent: all of them, until the conversation
    /// has been cut down to fit, and from then on what the cut left.
    pub fn context(&self) -> Vec<Message> {
        compact::apply(&self.messages, self.view)
    }

    /// How much of the transcript the model is sent.
    pub fn view(&self) -> View {
        self.view
    }

    /// Cuts down what the model is sent. The transcript itself is only added
    /// to: a marker says where the view now starts.
    pub fn compact(&mut self, view: View) -> Result<()> {
        let record = Record::Compacted {
            at: Utc::now(),
            start: view.start,
            trim_before: view.trim_before,
        };
        jsonl::append(&self.dir.join(TRANSCRIPT), &record)?;
        self.view = view;
        Ok(())
    }

    /// Adds a message to the transcript. It is on disk before this returns.
    pub fn push(&mut self, message: Message) -> Result<()> {
        let record = Record::Message {
            at: Utc::now(),
            message,
        };
        jsonl::append(&self.dir.join(TRANSCRIPT), &record)?;
        let Record::Message { message, .. } = record else {
            return Ok(());
        };
        if self.meta.title.is_empty() && message.role == Role::User {
            self.meta.title = title_of(&message.content);
            self.write_meta()?;
        }
        self.messages.push(message);
        Ok(())
    }

    /// Adds an event to the event file and returns its number. Events that
    /// are not durable get no number and are not written.
    pub fn log(&mut self, event: &Event) -> Result<Option<u64>> {
        if !event.is_durable() {
            return Ok(None);
        }
        let stamped = Stamped {
            seq: self.seq + 1,
            at: Utc::now(),
            event: event.clone(),
        };
        jsonl::append(&self.dir.join(EVENTS), &stamped)?;
        self.seq = stamped.seq;
        Ok(Some(stamped.seq))
    }

    /// The durable events numbered above `after`, oldest first.
    pub fn events_after(&self, after: u64) -> Result<Vec<Stamped>> {
        let mut events: Vec<Stamped> = jsonl::read(&self.dir.join(EVENTS))?;
        events.retain(|stamped| stamped.seq > after);
        Ok(events)
    }

    /// Answers any tool call the last assistant message made that has no
    /// result after it. A turn that died between a call and its result would
    /// otherwise leave a transcript no provider accepts.
    pub fn answer_unanswered(&mut self, why: &str) -> Result<usize> {
        let Some(asked_at) = self
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        else {
            return Ok(0);
        };
        let answered: Vec<&str> = self.messages[asked_at + 1..]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = self.messages[asked_at]
            .tool_calls
            .iter()
            .filter(|call| !answered.contains(&call.id.as_str()))
            .map(|call| call.id.clone())
            .collect();
        // A result must follow its call directly, so this only works while
        // nothing else has been said since.
        if self.messages[asked_at + 1..]
            .iter()
            .any(|m| m.role != Role::Tool)
        {
            return Ok(0);
        }
        for id in &missing {
            self.push(Message::tool(id, why))?;
        }
        Ok(missing.len())
    }

    /// Closes a turn that was cut short. If the event file ends in the middle
    /// of a turn, the process that was running it is gone, and nothing will
    /// ever finish it; a client shown such a session would wait for ever.
    /// Returns whether there was one.
    pub fn close_interrupted(&mut self) -> Result<bool> {
        let events: Vec<Stamped> = jsonl::read(&self.dir.join(EVENTS))?;
        let open = match events.last() {
            Some(last) => !matches!(last.event, Event::TurnFinished { .. }),
            None => false,
        };
        if open {
            self.answer_unanswered("not run: Tiphys stopped before this call was dealt with")?;
            self.log(&Event::TurnFinished {
                reason: StopReason::Failed,
                error: Some(INTERRUPTED.into()),
            })?;
        }
        Ok(open)
    }

    fn write_meta(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.meta)
            .map_err(|e| Error::Io(format!("cannot encode session meta: {e}")))?;
        write_atomic(&self.dir.join(META), &json, SHARED_FILE)
    }
}

/// Every session in `home`, newest first. A directory that is not a readable
/// session is skipped.
pub fn list(home: &Path) -> Result<Vec<Meta>> {
    let dir = home.join(SESSIONS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(format!("{}: {e}", dir.display()))),
    };
    let mut sessions: Vec<Meta> = entries
        .filter_map(|entry| read_meta(&entry.ok()?.path()).ok().flatten())
        .collect();
    // Ids are time-ordered, which settles two sessions made in one instant.
    sessions.sort_by(|a, b| (b.created, &b.id).cmp(&(a.created, &a.id)));
    Ok(sessions)
}

/// The session an audience was last in, if it has had one.
pub fn latest(home: &Path, audience: &str) -> Result<Option<String>> {
    Ok(list(home)?
        .into_iter()
        .find(|meta| meta.audience == audience)
        .map(|meta| meta.id))
}

/// What happened in a session, for a client that has just attached: its
/// durable events, oldest first. A long session is cut to its latest part,
/// with a note where the cut is. A session that cannot be read has no history.
pub fn history(home: &Path, id: &str) -> Vec<Event> {
    let path = home.join(SESSIONS_DIR).join(id).join(EVENTS);
    let events: Vec<Stamped> = jsonl::read(&path).unwrap_or_default();
    let skipped = events.len().saturating_sub(HISTORY_EVENTS);
    let mut history = Vec::with_capacity(events.len() - skipped + 1);
    if skipped > 0 {
        history.push(Event::Notice {
            text: format!("{skipped} earlier events of this session are not shown."),
        });
    }
    history.extend(
        events
            .into_iter()
            .skip(skipped)
            .map(|stamped| stamped.event),
    );
    history
}

fn read_meta(dir: &Path) -> Result<Option<Meta>> {
    let path = dir.join(META);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| Error::Io(format!("{}: {e}", path.display())))
}

/// The first line of a message, cut to a length that fits a list.
fn title_of(text: &str) -> String {
    const LIMIT: usize = 60;
    let line = text.trim().lines().next().unwrap_or_default().trim();
    if line.chars().count() <= LIMIT {
        line.to_string()
    } else {
        let cut: String = line.chars().take(LIMIT - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::proto::StopReason;

    fn opening() -> Opening {
        Opening {
            connection: "openrouter".into(),
            model: "vendor/model".into(),
            audience: "terminal".into(),
            system: "You are Tiphys.\n".into(),
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Read a file.".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn a_session_reopens_with_its_prompt_tools_and_history_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let mut session = Session::create(home.path(), opening()).unwrap();
        let id = session.meta().id.clone();
        session
            .push(Message::user("how full is the disk?\nplease check"))
            .unwrap();
        session
            .push(Message::assistant("", vec![call("a")]))
            .unwrap();
        session.push(Message::tool("a", "42%")).unwrap();
        session
            .push(Message::assistant("42% full.", vec![]))
            .unwrap();

        let reopened = Session::open(home.path(), &id).unwrap();
        assert_eq!(reopened.system(), "You are Tiphys.\n");
        assert_eq!(reopened.tools(), session.tools());
        assert_eq!(reopened.messages(), session.messages());
        assert_eq!(reopened.meta().title, "how full is the disk?");
        assert_eq!(reopened.meta(), session.meta());
    }

    #[test]
    fn durable_events_are_numbered_and_can_be_replayed_from_any_point() {
        let home = tempfile::tempdir().unwrap();
        let mut session = Session::create(home.path(), opening()).unwrap();
        let finished = Event::TurnFinished {
            reason: StopReason::Completed,
            error: None,
        };
        assert_eq!(
            session
                .log(&Event::UserMessage { text: "hi".into() })
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            session.log(&Event::Text { text: "he".into() }).unwrap(),
            None
        );
        assert_eq!(
            session
                .log(&Event::AssistantMessage {
                    text: "hello".into()
                })
                .unwrap(),
            Some(2)
        );
        assert_eq!(session.log(&finished).unwrap(), Some(3));

        let missed = session.events_after(1).unwrap();
        assert_eq!(missed.iter().map(|s| s.seq).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(missed[1].event, finished);

        // Numbering carries on after a restart.
        let mut reopened = Session::open(home.path(), &session.meta().id).unwrap();
        assert_eq!(
            reopened
                .log(&Event::Notice {
                    text: "back".into()
                })
                .unwrap(),
            Some(4)
        );
    }

    #[test]
    fn calls_left_without_a_result_are_answered() {
        let home = tempfile::tempdir().unwrap();
        let mut session = Session::create(home.path(), opening()).unwrap();
        session.push(Message::user("go")).unwrap();
        assert_eq!(session.answer_unanswered("stopped").unwrap(), 0);

        session
            .push(Message::assistant(
                "",
                vec![call("a"), call("b"), call("c")],
            ))
            .unwrap();
        session.push(Message::tool("a", "done")).unwrap();
        assert_eq!(session.answer_unanswered("stopped").unwrap(), 2);
        let tail: Vec<_> = session.messages()[2..]
            .iter()
            .map(|m| (m.tool_call_id.as_deref().unwrap(), m.content.as_str()))
            .collect();
        assert_eq!(tail, [("a", "done"), ("b", "stopped"), ("c", "stopped")]);
        // Nothing left to answer.
        assert_eq!(session.answer_unanswered("stopped").unwrap(), 0);
    }

    #[test]
    fn sessions_are_listed_newest_first_and_broken_ones_are_skipped() {
        let home = tempfile::tempdir().unwrap();
        assert!(list(home.path()).unwrap().is_empty());
        let first = Session::create(home.path(), opening()).unwrap();
        let second = Session::create(home.path(), opening()).unwrap();
        std::fs::create_dir_all(home.path().join("sessions/not-a-session")).unwrap();
        std::fs::create_dir_all(home.path().join("sessions/bad")).unwrap();
        std::fs::write(home.path().join("sessions/bad/meta.json"), "{").unwrap();

        let ids: Vec<String> = list(home.path())
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, [second.meta().id.clone(), first.meta().id.clone()]);
    }

    #[test]
    fn a_turn_cut_short_is_closed_once_and_a_finished_one_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let mut session = Session::create(home.path(), opening()).unwrap();
        assert!(!session.close_interrupted().unwrap());

        session.push(Message::user("go")).unwrap();
        session
            .log(&Event::UserMessage { text: "go".into() })
            .unwrap();
        session
            .push(Message::assistant("", vec![call("a")]))
            .unwrap();
        session
            .log(&Event::Notice {
                text: "working".into(),
            })
            .unwrap();
        // The process dies here. Later, the session is opened again.
        let mut reopened = Session::open(home.path(), &session.meta().id).unwrap();
        assert!(reopened.close_interrupted().unwrap());
        assert!(!reopened.close_interrupted().unwrap());

        let events = history(home.path(), &session.meta().id);
        assert_eq!(
            events.last(),
            Some(&Event::TurnFinished {
                reason: StopReason::Failed,
                error: Some(INTERRUPTED.into())
            })
        );
        assert_eq!(events.len(), 3);
        // The call it left open has its result, so the next turn can be sent.
        assert_eq!(
            reopened.messages().last().unwrap().tool_call_id.as_deref(),
            Some("a")
        );
    }

    #[test]
    fn the_latest_session_is_found_per_audience_and_history_is_cut_to_its_end() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(latest(home.path(), "terminal").unwrap(), None);
        let terminal = Session::create(home.path(), opening()).unwrap();
        let mut chat = Session::create(
            home.path(),
            Opening {
                audience: "telegram:42".into(),
                ..opening()
            },
        )
        .unwrap();
        assert_eq!(
            latest(home.path(), "terminal").unwrap(),
            Some(terminal.meta().id.clone())
        );
        assert_eq!(
            latest(home.path(), "telegram:42").unwrap(),
            Some(chat.meta().id.clone())
        );
        assert_eq!(latest(home.path(), "telegram:7").unwrap(), None);

        assert!(history(home.path(), &terminal.meta().id).is_empty());
        assert!(history(home.path(), "0199aaaa-0000-7000-8000-000000000000").is_empty());
        for n in 0..HISTORY_EVENTS + 25 {
            chat.log(&Event::Notice {
                text: format!("event {n}"),
            })
            .unwrap();
        }
        let events = history(home.path(), &chat.meta().id);
        assert_eq!(events.len(), HISTORY_EVENTS + 1);
        assert_eq!(
            events[0],
            Event::Notice {
                text: "25 earlier events of this session are not shown.".into()
            }
        );
        assert_eq!(
            events[1],
            Event::Notice {
                text: "event 25".into()
            }
        );
    }

    #[test]
    fn cutting_the_view_down_keeps_every_message_and_survives_a_reopen() {
        let home = tempfile::tempdir().unwrap();
        let mut session = Session::create(home.path(), opening()).unwrap();
        for n in 0..3 {
            session
                .push(Message::user(format!("question {n}")))
                .unwrap();
            session
                .push(Message::assistant("", vec![call(&format!("c{n}"))]))
                .unwrap();
            session
                .push(Message::tool(format!("c{n}"), "a long result"))
                .unwrap();
            session
                .push(Message::assistant(format!("answer {n}"), vec![]))
                .unwrap();
        }
        assert_eq!(session.context(), session.messages());

        session
            .compact(View {
                start: 4,
                trim_before: 8,
            })
            .unwrap();
        session.push(Message::user("question 3")).unwrap();

        let reopened = Session::open(home.path(), &session.meta().id).unwrap();
        assert_eq!(reopened.messages().len(), 13);
        assert_eq!(
            reopened.view(),
            View {
                start: 4,
                trim_before: 8
            }
        );
        let context = reopened.context();
        // A note where the cut is, then the second turn with its result thinned.
        assert_eq!(context.len(), 10);
        assert!(context[0].content.contains("first 4 messages"));
        assert_eq!(context[1].content, "question 1");
        assert_eq!(context[3].content, compact::TRIMMED);
        assert_eq!(context[7].content, "a long result");
        assert_eq!(context[9].content, "question 3");
    }

    #[test]
    fn an_id_that_is_not_one_never_becomes_a_path() {
        let home = tempfile::tempdir().unwrap();
        for id in ["", "../keys", "a/b", "not a session"] {
            let err = Session::open(home.path(), id).unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{id}");
        }
        let err = Session::open(home.path(), "0199aaaa-0000-7000-8000-000000000000").unwrap_err();
        assert!(err.to_string().contains("no session"));
    }

    #[test]
    fn a_title_is_the_first_line_cut_to_fit() {
        assert_eq!(title_of("  short\nmore"), "short");
        let long = "word ".repeat(30);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), 60);
        assert!(title.ends_with('…'));
    }
}
