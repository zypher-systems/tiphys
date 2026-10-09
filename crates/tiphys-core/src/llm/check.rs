//! Proving a model works before it is relied on.
//!
//! A model list says a model exists. It does not say the key is accepted for
//! it, or that it can call tools, and an agent that cannot call tools can do
//! nothing. So before a model is saved, it is asked to make one real call and
//! to read the result: the same round trip every turn is made of.

use serde_json::json;

use super::{Message, Provider, Request, ToolSpec, collect};
use crate::{Error, Result};

const TOOL: &str = "ping";
const WORD: &str = "anchor";
/// Enough for a model that thinks before it answers; small enough to bound
/// what the check can cost.
const MAX_TOKENS: u32 = 2048;

/// Asks `model` to call a tool and then to read what it returned. An error
/// says which half failed, in words for the owner.
pub async fn tool_round_trip(provider: &dyn Provider, model: &str) -> Result<()> {
    let mut request = Request {
        model: model.to_string(),
        system: Some("This is a connection check from Tiphys. Do exactly what is asked.".into()),
        messages: vec![Message::user(format!(
            "Call the `{TOOL}` tool with the word \"{WORD}\". Do not answer in text."
        ))],
        tools: vec![ToolSpec {
            name: TOOL.into(),
            description: "Checks that tool calls work. Returns the word it was given.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"word": {"type": "string"}},
                "required": ["word"],
            }),
        }],
        max_tokens: Some(MAX_TOKENS),
    };
    let reply = collect(provider.stream(request.clone()).await?).await?;
    let Some(call) = reply.tool_calls.iter().find(|call| call.name == TOOL) else {
        let said: String = reply.text.trim().chars().take(120).collect();
        return Err(Error::Provider(if said.is_empty() {
            "the model did not call the tool it was asked to call; it may not support tool calls"
                .into()
        } else {
            format!(
                "the model answered in text instead of calling the tool it was asked to call; \
                 it may not support tool calls. It said: {said}"
            )
        }));
    };
    let args = call
        .args()
        .map_err(|e| Error::Provider(format!("the model called the tool, but its {e}")))?;
    let word = args
        .get("word")
        .and_then(|w| w.as_str())
        .unwrap_or_default();

    request.messages.push(Message::assistant(
        reply.text.clone(),
        reply.tool_calls.clone(),
    ));
    // Every call needs a result, should the model have made more than one.
    for call in &reply.tool_calls {
        request
            .messages
            .push(Message::tool(&call.id, format!("pong: {word}")));
    }
    let read_back = async { collect(provider.stream(request).await?).await };
    read_back.await.map_err(|e| {
        Error::Provider(format!(
            "the model called the tool, but reading the result back failed: {e}"
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Delta, ReplayProvider, Role, ToolCallPart};

    fn calls(name: &str, arguments: &str) -> Vec<Delta> {
        vec![
            Delta::ToolCall(ToolCallPart {
                slot: Some(0),
                id: "call_1".into(),
                name: name.into(),
                arguments: arguments.into(),
            }),
            Delta::Done,
        ]
    }

    fn says(text: &str) -> Vec<Delta> {
        vec![Delta::Text(text.into()), Delta::Done]
    }

    #[tokio::test]
    async fn a_model_that_calls_the_tool_and_reads_the_result_passes() {
        let provider =
            ReplayProvider::new(vec![calls("ping", r#"{"word":"anchor"}"#), says("pong")]);
        tool_round_trip(&provider, "vendor/model").await.unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].model, "vendor/model");
        assert_eq!(requests[0].tools[0].name, "ping");
        // The second request carries the call and its result.
        let last = requests[1].messages.last().unwrap();
        assert_eq!(
            (last.role, last.content.as_str()),
            (Role::Tool, "pong: anchor")
        );
        assert_eq!(last.tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn each_way_of_failing_is_told_apart() {
        let cases: Vec<(ReplayProvider, &str)> = vec![
            (
                ReplayProvider::new(vec![says("Sure! Pinging anchor.")]),
                "answered in text instead of calling the tool",
            ),
            (
                ReplayProvider::new(vec![vec![Delta::Done]]),
                "did not call the tool",
            ),
            (
                ReplayProvider::new(vec![calls("ping", "{\"word\":")]),
                "its arguments are not JSON",
            ),
            (
                ReplayProvider::default().then_fail("the provider refused the key (401)"),
                "refused the key",
            ),
            (
                ReplayProvider::new(vec![calls("ping", "{}")])
                    .then_fail("the provider answered 500"),
                "reading the result back failed: the provider answered 500",
            ),
        ];
        for (provider, expected) in cases {
            let err = tool_round_trip(&provider, "m")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{err}");
        }
    }
}
