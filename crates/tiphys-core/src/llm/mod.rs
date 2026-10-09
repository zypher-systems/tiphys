//! Talking to a model.
//!
//! The types here are Tiphys's own and belong to no wire format: a
//! [`Request`] goes in, a stream of [`Delta`]s comes out, and a
//! [`ReplyBuilder`] folds the stream into one [`Reply`]. A wire format is a
//! [`Provider`]; [`chat`] is the first, and [`ReplayProvider`] plays back
//! scripted or captured streams for tests.

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};

use crate::spend::{Rates, Usage};
use crate::{Error, Result};

pub mod catalog;
pub mod chat;
mod replay;
mod sse;

pub use replay::ReplayProvider;

/// Who a message is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// The result of a tool call, answering the call named by `tool_call_id`.
    Tool,
}

/// One message of a conversation. This is also the shape a transcript stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// Calls the assistant made in this message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For a tool message: the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// A call the model made. `arguments` is the JSON text exactly as it was
/// sent, so what is stored and sent back is what the model wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// The arguments as a JSON value. No arguments at all is an empty object.
    pub fn args(&self) -> std::result::Result<serde_json::Value, String> {
        if self.arguments.trim().is_empty() {
            return Ok(serde_json::Value::Object(Default::default()));
        }
        serde_json::from_str(&self.arguments).map_err(|e| format!("arguments are not JSON: {e}"))
    }
}

/// A tool as the model is told about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// A JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// One completion to ask for.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
}

/// One piece of a reply as it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// Text of the answer.
    Text(String),
    /// The model's thinking, where a provider shows it. For display only.
    Reasoning(String),
    /// Part of a tool call.
    ToolCall(ToolCallPart),
    /// Token counts. A provider may send them more than once; the last wins.
    Usage(Usage),
    /// What the provider says the call cost, in dollars.
    Cost(f64),
    /// The reply stopped at the output limit and is not whole.
    CutOff,
    /// The reply is complete. Nothing follows.
    Done,
}

/// A fragment of a tool call. The id and name usually come once, then the
/// arguments in pieces that carry neither.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolCallPart {
    /// The provider's position for the call this fragment belongs to.
    pub slot: Option<u32>,
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A model a connection offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// The context window in tokens, where the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// The price, where the provider lists one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rates: Option<Rates>,
    /// Whether it takes tool calls, where the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
}

/// The stream a provider returns.
pub type DeltaStream = Pin<Box<dyn Stream<Item = Result<Delta>> + Send>>;

/// A model endpoint, in one wire format.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Starts a completion. An error here means nothing was streamed; an
    /// error inside the stream means the reply so far is not to be trusted.
    async fn stream(&self, request: Request) -> Result<DeltaStream>;

    /// The models this connection can chat with.
    async fn models(&self) -> Result<Vec<Model>>;
}

/// A whole reply.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reply {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    /// The provider's own figure for the call, in dollars.
    pub cost: Option<f64>,
    /// The reply hit the output limit. Its last tool call may be incomplete.
    pub cut_off: bool,
}

/// Folds a stream of deltas into a [`Reply`].
#[derive(Debug, Default)]
pub struct ReplyBuilder {
    reply: Reply,
    calls: CallAssembler,
    done: bool,
}

impl ReplyBuilder {
    pub fn push(&mut self, delta: Delta) {
        match delta {
            Delta::Text(text) => self.reply.text.push_str(&text),
            Delta::Reasoning(text) => self.reply.reasoning.push_str(&text),
            Delta::ToolCall(part) => self.calls.push(part),
            Delta::Usage(usage) => self.reply.usage = Some(usage),
            Delta::Cost(cost) => self.reply.cost = Some(cost),
            Delta::CutOff => self.reply.cut_off = true,
            Delta::Done => self.done = true,
        }
    }

    /// The reply, if the stream said it was complete. A stream that just
    /// stops has lost its end, and what arrived cannot be acted on.
    pub fn finish(self) -> Result<Reply> {
        if !self.done {
            return Err(Error::Provider(
                "the reply ended before the provider said it was complete".into(),
            ));
        }
        let mut reply = self.reply;
        reply.tool_calls = self.calls.finish();
        Ok(reply)
    }
}

/// Puts tool calls back together from their fragments.
///
/// Providers number parallel calls and interleave their arguments, so a
/// fragment is matched to its call by slot. Where a slot and an id disagree
/// the id decides: some servers give every parallel call slot 0 and tell them
/// apart by id alone.
#[derive(Debug, Default)]
struct CallAssembler {
    calls: Vec<Building>,
    by_slot: HashMap<u32, usize>,
    current: Option<usize>,
}

#[derive(Debug, Default)]
struct Building {
    /// Empty until the provider names one.
    id: String,
    name: String,
    arguments: String,
}

impl CallAssembler {
    fn push(&mut self, part: ToolCallPart) {
        let by_id = || {
            self.calls
                .iter()
                .position(|call| !part.id.is_empty() && call.id == part.id)
        };
        let target = match part.slot {
            Some(slot) => match self.by_slot.get(&slot).copied() {
                Some(index)
                    if !part.id.is_empty()
                        && !self.calls[index].id.is_empty()
                        && self.calls[index].id != part.id =>
                {
                    by_id()
                }
                found => found,
            },
            None if part.id.is_empty() => self.current,
            None => by_id(),
        };
        let index = target.unwrap_or_else(|| {
            self.calls.push(Building::default());
            self.calls.len() - 1
        });
        let call = &mut self.calls[index];
        if !part.id.is_empty() {
            call.id = part.id;
        }
        if !part.name.is_empty() {
            call.name = part.name;
        }
        call.arguments.push_str(&part.arguments);
        if let Some(slot) = part.slot {
            self.by_slot.insert(slot, index);
        }
        self.current = Some(index);
    }

    /// The calls in the order they first appeared. A call that never got a
    /// name cannot be run and is dropped; one that never got an id is given
    /// one, since its result has to answer something.
    fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .enumerate()
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{}", index + 1)
                } else {
                    call.id
                },
                name: call.name,
                arguments: call.arguments,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(slot: Option<u32>, id: &str, name: &str, arguments: &str) -> Delta {
        Delta::ToolCall(ToolCallPart {
            slot,
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        })
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn fold(deltas: Vec<Delta>) -> Reply {
        let mut builder = ReplyBuilder::default();
        for delta in deltas {
            builder.push(delta);
        }
        builder.finish().unwrap()
    }

    #[test]
    fn a_reply_is_its_text_its_calls_and_the_last_usage_sent() {
        let usage = |input| Usage {
            input,
            output: 3,
            ..Usage::default()
        };
        let reply = fold(vec![
            Delta::Reasoning("hm".into()),
            Delta::Text("Hel".into()),
            Delta::Usage(usage(5)),
            Delta::Text("lo".into()),
            Delta::Usage(usage(12)),
            Delta::Cost(0.002),
            Delta::Done,
        ]);
        assert_eq!(reply.text, "Hello");
        assert_eq!(reply.reasoning, "hm");
        assert_eq!(reply.usage, Some(usage(12)));
        assert_eq!(reply.cost, Some(0.002));
        assert!(!reply.cut_off && reply.tool_calls.is_empty());
    }

    #[test]
    fn a_stream_that_just_stops_is_not_a_reply() {
        let mut builder = ReplyBuilder::default();
        builder.push(Delta::Text("half an ans".into()));
        assert!(matches!(builder.finish(), Err(Error::Provider(_))));
    }

    #[test]
    fn tool_calls_are_put_together_however_the_provider_sends_them() {
        let cases: Vec<(&str, Vec<Delta>, Vec<ToolCall>)> = vec![
            (
                "one call, arguments in pieces",
                vec![
                    part(Some(0), "a", "read_file", ""),
                    part(Some(0), "", "", "{\"path\":"),
                    part(Some(0), "", "", "\"a.txt\"}"),
                ],
                vec![call("a", "read_file", "{\"path\":\"a.txt\"}")],
            ),
            (
                "two calls interleaved by slot",
                vec![
                    part(Some(0), "a", "read_file", ""),
                    part(Some(1), "b", "list_dir", ""),
                    part(Some(0), "", "", "{\"path\":"),
                    part(Some(1), "", "", "{\"path\":"),
                    part(Some(0), "", "", "\"a\"}"),
                    part(Some(1), "", "", "\"b\"}"),
                ],
                vec![
                    call("a", "read_file", "{\"path\":\"a\"}"),
                    call("b", "list_dir", "{\"path\":\"b\"}"),
                ],
            ),
            (
                "every call in slot 0, told apart by id",
                vec![
                    part(Some(0), "a", "read_file", "{\"path\":\"a\"}"),
                    part(Some(0), "b", "read_file", "{\"path\":\"b\"}"),
                ],
                vec![
                    call("a", "read_file", "{\"path\":\"a\"}"),
                    call("b", "read_file", "{\"path\":\"b\"}"),
                ],
            ),
            (
                "arguments before the name",
                vec![
                    part(Some(0), "", "", "{\"path\":"),
                    part(Some(0), "a", "read_file", "\"a\"}"),
                ],
                vec![call("a", "read_file", "{\"path\":\"a\"}")],
            ),
            (
                "no slots: an id starts a call and bare pieces continue it",
                vec![
                    part(None, "a", "read_file", "{\"pa"),
                    part(None, "", "", "th\":\"a\"}"),
                    part(None, "b", "list_dir", "{}"),
                ],
                vec![
                    call("a", "read_file", "{\"path\":\"a\"}"),
                    call("b", "list_dir", "{}"),
                ],
            ),
            (
                "no id given: one is made up; no name given: dropped",
                vec![
                    part(Some(0), "", "", "{}"),
                    part(Some(1), "", "list_dir", "{}"),
                ],
                vec![call("call_1", "list_dir", "{}")],
            ),
        ];
        for (what, mut deltas, expected) in cases {
            deltas.push(Delta::Done);
            assert_eq!(fold(deltas).tool_calls, expected, "{what}");
        }
    }

    #[test]
    fn arguments_parse_as_json_and_none_at_all_is_an_empty_object() {
        assert_eq!(call("a", "t", " ").args().unwrap(), serde_json::json!({}));
        assert_eq!(
            call("a", "t", "{\"path\":\"x\"}").args().unwrap(),
            serde_json::json!({"path": "x"})
        );
        assert!(call("a", "t", "{\"path\":").args().is_err());
    }

    #[test]
    fn a_message_is_stored_without_the_fields_it_does_not_use() {
        let user = serde_json::to_string(&Message::user("hi")).unwrap();
        assert_eq!(user, r#"{"role":"user","content":"hi"}"#);
        let result = serde_json::to_string(&Message::tool("a", "ok")).unwrap();
        assert_eq!(
            result,
            r#"{"role":"tool","content":"ok","tool_call_id":"a"}"#
        );
        let calling = Message::assistant("", vec![call("a", "t", "{}")]);
        let stored = serde_json::to_string(&calling).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&stored).unwrap(), calling);
    }
}
