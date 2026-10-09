//! What the agent tells whoever is watching.
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

use crate::spend::Usage;

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
}

impl Event {
    /// Whether this event is kept in the session's event file.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Self::Text { .. } | Self::Reasoning { .. })
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
    fn the_pieces_of_a_streaming_reply_are_not_kept() {
        assert!(!Event::Text { text: "a".into() }.is_durable());
        assert!(!Event::Reasoning { text: "a".into() }.is_durable());
        assert!(Event::AssistantMessage { text: "a".into() }.is_durable());
        assert!(Event::Notice { text: "a".into() }.is_durable());
    }
}
