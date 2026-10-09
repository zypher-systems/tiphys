//! What a client and the agent's host say to each other.
//!
//! A client sends [`Request`]s. The terminal app is the first client; from
//! the daemon onward, chat adapters are clients too.
//!
//! A turn is reported as a stream of [`Event`]s. The terminal app draws
//! them, a one-shot run prints them, and from the daemon onward they are what
//! travels to a client, one JSON object per line:
//!
//! ```json
//! {"kind":"tool_started","id":"call_1","tool":"read_file","summary":"read /etc/hostname","reason":"to learn the host's name"}
//! ```
//!
//! Durable events are also appended to the session's `events.jsonl`, so a
//! client that was away can catch up. The pieces of a reply as it streams are
//! not: the finished message is.

use serde::{Deserialize, Serialize};

use crate::config::Connection;
use crate::keys::Secret;
use crate::llm::Model;
use crate::policy::Class;
use crate::report::Report;
use crate::spend::Usage;

/// What a client asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Say where things stand. Answered with [`Event::State`].
    Hello,
    /// Run a turn with this message. One that arrives during a turn waits
    /// its turn.
    Prompt { text: String },
    /// Stop the turn that is running.
    Cancel,
    /// Answer the approval request `id`. With `approve` false, `note` is
    /// passed on to the model as the owner's reason.
    Approval {
        id: String,
        approve: bool,
        note: Option<String>,
    },
    /// Leave the current session; the next prompt begins a new one.
    NewSession,
    /// Reach a connection that is being set up and list its models. Answered
    /// with [`Event::Models`] or [`Event::Failed`].
    TryConnection(Draft),
    /// Make a real tool-call round trip on the draft's model. Answered with
    /// [`Event::ModelChecked`].
    CheckModel(Draft),
    /// Keep a connection, with its key if one was typed, and make it the
    /// default. Answered with [`Event::State`].
    SaveConnection(Draft),
    /// List the models of a connection that is already set up.
    Models { connection: String },
    /// Use this model for new sessions on a connection. Answered with
    /// [`Event::State`].
    ChooseModel { connection: String, model: String },
    /// Say something about Tiphys itself: its action log, what it has spent,
    /// its sessions, whether it is in order. Answered with [`Event::Report`].
    Report(Report),
}

/// A connection as it is being set up: not yet saved, with the key as typed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub name: String,
    pub connection: Connection,
    /// The key typed in the app. `None` means use the one already stored
    /// under this name, if there is one.
    pub key: Option<Secret>,
}

/// Where things stand.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct State {
    pub connections: Vec<ConnectionInfo>,
    /// The connection a new session uses, when that is settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// The session prompts go to. None until the first prompt begins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
}

/// A connection, as much of it as a client may see. The key is never part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub name: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub local: bool,
    /// Whether a key is stored or supplied for it.
    pub has_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub connection: String,
    pub model: String,
}

/// One thing that happened in a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// The turn began with this message.
    UserMessage { text: String },
    /// A piece of the answer as it streams.
    Text { text: String },
    /// A piece of the model's thinking as it streams.
    Reasoning { text: String },
    /// The whole text the model wrote in one round.
    AssistantMessage { text: String },
    /// A tool call is about to be dealt with.
    ToolStarted {
        id: String,
        tool: String,
        /// What it will do, in a line.
        summary: String,
        /// Why, in the model's words.
        reason: String,
    },
    /// A tool call was dealt with. `output` is the start of what it returned.
    ToolFinished {
        id: String,
        ok: bool,
        output: String,
    },
    /// An action is waiting for the owner's yes or no.
    ApprovalRequested {
        id: String,
        tool: String,
        summary: String,
        reason: String,
        /// Why it has to ask, in the rules' words.
        why: String,
        class: Class,
        /// What will change, when there is something to show.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    /// The owner answered, or the question ran out of time.
    ApprovalResolved {
        id: String,
        approved: bool,
        /// What the model was told, when the answer was no.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
    /// A model call was paid for. An unknown cost is absent, not zero.
    Spend {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Something the owner should know that is not part of the answer.
    Notice { text: String },
    /// The turn is over.
    TurnFinished {
        reason: StopReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Where things stand, after a request that asked or changed it.
    State(State),
    /// The conversation so far, for a client that has just attached or whose
    /// session has just changed. It replaces whatever the client was showing.
    History { events: Vec<Event> },
    /// The models a connection offers.
    Models { models: Vec<Model> },
    /// How a model's tool-call round trip went.
    ModelChecked {
        model: String,
        ok: bool,
        message: String,
    },
    /// A report that was asked for, as text. `ok` is whether what it found is
    /// good news.
    Report { text: String, ok: bool },
    /// A request could not be carried out.
    Failed { message: String },
}

impl Event {
    /// Whether this event is kept in the session's event file. The pieces of
    /// a streaming reply are not, and neither are answers to a client's own
    /// requests, which are not part of any session.
    pub fn is_durable(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::AssistantMessage { .. }
                | Self::ToolStarted { .. }
                | Self::ToolFinished { .. }
                | Self::ApprovalRequested { .. }
                | Self::ApprovalResolved { .. }
                | Self::Spend { .. }
                | Self::Notice { .. }
                | Self::TurnFinished { .. }
        )
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model answered and called no more tools.
    #[default]
    Completed,
    /// The round limit was reached with work still going.
    Rounds,
    /// The same call kept giving the same result.
    Stuck,
    /// The reply kept hitting the output limit.
    CutOff,
    /// The owner stopped it.
    Cancelled,
    /// The provider or the disk failed. The event carries the error.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_travel_as_tagged_json_and_come_back_the_same() {
        let events = [
            Event::UserMessage { text: "hi".into() },
            Event::ToolStarted {
                id: "call_1".into(),
                tool: "read_file".into(),
                summary: "read /etc/hostname".into(),
                reason: "to learn the host's name".into(),
            },
            Event::Spend {
                cost: None,
                usage: None,
            },
            Event::TurnFinished {
                reason: StopReason::CutOff,
                error: None,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(
                serde_json::from_str::<Event>(&json).unwrap(),
                event,
                "{json}"
            );
        }
        assert_eq!(
            serde_json::to_string(&Event::Spend {
                cost: None,
                usage: None
            })
            .unwrap(),
            r#"{"kind":"spend"}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::TurnFinished {
                reason: StopReason::CutOff,
                error: None
            })
            .unwrap(),
            r#"{"kind":"turn_finished","reason":"cut_off"}"#
        );
    }

    #[test]
    fn requests_travel_as_tagged_json_and_a_typed_key_arrives_as_typed() {
        let draft = Draft {
            name: "work".into(),
            connection: Connection {
                base_url: "https://api.example.com/v1".into(),
                model: Some("m".into()),
                env_key: None,
                local: false,
            },
            key: Secret::new("sk-typed"),
        };
        let requests = [
            Request::Hello,
            Request::Prompt { text: "hi".into() },
            Request::Cancel,
            Request::Approval {
                id: "w1".into(),
                approve: false,
                note: Some("no".into()),
            },
            Request::NewSession,
            Request::TryConnection(Draft {
                key: None,
                ..draft.clone()
            }),
            Request::SaveConnection(draft),
            Request::Models {
                connection: "work".into(),
            },
            Request::ChooseModel {
                connection: "work".into(),
                model: "m".into(),
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&json).unwrap(),
                request,
                "{json}"
            );
        }
        assert_eq!(
            serde_json::to_string(&Request::Cancel).unwrap(),
            r#"{"kind":"cancel"}"#
        );
        // The key crosses the wire, and nothing prints it on the way.
        let sent: Request = serde_json::from_str(
            r#"{"kind":"save_connection","name":"work","connection":{"base_url":"https://a.example"},"key":"sk-typed"}"#,
        )
        .unwrap();
        let Request::SaveConnection(received) = &sent else {
            panic!("expected a connection to save");
        };
        assert_eq!(received.key.as_ref().unwrap().expose(), "sk-typed");
        assert!(!format!("{sent:?}").contains("sk-typed"));
        assert!(serde_json::from_str::<Request>(r#"{"kind":"save_connection","name":"w","connection":{"base_url":"https://a.example"},"key":""}"#).is_err());
    }

    #[test]
    fn the_pieces_of_a_streaming_reply_are_not_kept() {
        assert!(!Event::Text { text: "a".into() }.is_durable());
        assert!(!Event::Reasoning { text: "a".into() }.is_durable());
        assert!(Event::AssistantMessage { text: "a".into() }.is_durable());
        assert!(Event::Notice { text: "a".into() }.is_durable());
        assert!(!Event::State(State::default()).is_durable());
        assert!(!Event::History { events: Vec::new() }.is_durable());
        assert!(
            !Event::Failed {
                message: "a".into()
            }
            .is_durable()
        );
    }
}
