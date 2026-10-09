//! The Chat Completions wire format.
//!
//! This is the format OpenRouter, OpenAI, Ollama, vLLM and most other
//! endpoints speak: `POST {base}/chat/completions` with `stream: true`, a
//! reply as server-sent events, and `GET {base}/models` for the model list.
//!
//! Endpoints agree on the outline and differ in the details, so the reader
//! is lenient about what it accepts: tool-call arguments as a string or an
//! object, reasoning under either of two names, cached-token counts in three
//! places, errors delivered as an ordinary chunk in a stream that began with
//! `200 OK`.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::sse::SseDecoder;
use super::{Delta, DeltaStream, Model, Provider, Request, Role, ToolCallPart};
use crate::config::Connection;
use crate::keys::Secret;
use crate::spend::{Rates, Usage};
use crate::{Error, Result};

/// How long to wait, and how often to try again.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// To open a connection.
    pub connect: Duration,
    /// For the next bytes of a response, keep-alives included.
    pub idle: Duration,
    /// For the next piece of the reply itself. Keep-alives reset `idle`, so a
    /// request stuck in a provider's queue would otherwise wait for ever.
    pub stall: Duration,
    /// Attempts after the first, for a failure that may pass.
    pub retries: u32,
    /// The first wait before trying again. It doubles each time.
    pub backoff: Duration,
}

impl Timing {
    /// For a provider on the internet.
    pub const REMOTE: Self = Self {
        connect: Duration::from_secs(30),
        idle: Duration::from_secs(120),
        stall: Duration::from_secs(300),
        retries: 3,
        backoff: Duration::from_millis(500),
    };
    /// For a server on the owner's hardware, which may load a model first.
    pub const LOCAL: Self = Self {
        idle: Duration::from_secs(600),
        stall: Duration::from_secs(900),
        ..Self::REMOTE
    };
}

/// The longest single wait between attempts, whatever the provider asks for.
const LONGEST_WAIT: Duration = Duration::from_secs(20);

/// A connection that speaks Chat Completions.
pub struct ChatProvider {
    client: reqwest::Client,
    base: String,
    headers: HeaderMap,
    local: bool,
    timing: Timing,
}

impl ChatProvider {
    /// A provider for `connection`, with its key if it has one.
    pub fn new(connection: &Connection, key: Option<&Secret>) -> Result<Self> {
        let timing = if connection.local {
            Timing::LOCAL
        } else {
            Timing::REMOTE
        };
        Self::with_timing(connection, key, timing)
    }

    /// [`ChatProvider::new`] with the waits chosen by the caller.
    pub fn with_timing(
        connection: &Connection,
        key: Option<&Secret>,
        timing: Timing,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(format!("tiphys/{}", crate::VERSION))
            .connect_timeout(timing.connect)
            .read_timeout(timing.idle)
            .build()
            .map_err(|e| Error::Provider(format!("could not set up the connection: {e}")))?;
        Ok(Self {
            client,
            base: connection.base().to_string(),
            headers: headers(connection, key)?,
            local: connection.local,
            timing,
        })
    }

    /// Sends a request, trying again while the failure is one that may pass.
    /// Only the opening of a request is retried: once a reply has started to
    /// stream, a second attempt would repeat text the caller already has.
    async fn send(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            let last = attempt == self.timing.retries;
            let wait = match build().headers(self.headers.clone()).send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let asked = retry_after(response.headers());
                    let body = response.text().await.unwrap_or_default();
                    if last || !may_pass(status.as_u16()) {
                        return Err(http_error(status, &body));
                    }
                    asked
                }
                // A local server that refuses the connection is not running,
                // and will not be in a few seconds.
                Err(e) if e.is_connect() && self.local => {
                    return Err(Error::Provider(format!(
                        "could not reach the local model server at {}; is it running?",
                        self.base
                    )));
                }
                Err(e) if last || !(e.is_timeout() || e.is_connect() || e.is_request()) => {
                    return Err(Error::Provider(format!(
                        "could not reach {}: {}",
                        self.base,
                        e.without_url()
                    )));
                }
                Err(_) => None,
            };
            tokio::time::sleep(wait.unwrap_or_else(|| backoff(self.timing.backoff, attempt))).await;
            attempt += 1;
        }
    }
}

#[async_trait]
impl Provider for ChatProvider {
    async fn stream(&self, request: Request) -> Result<DeltaStream> {
        let url = format!("{}/chat/completions", self.base);
        let body = body(&request);
        let response = self.send(|| self.client.post(&url).json(&body)).await?;
        Ok(Box::pin(deltas(response.bytes_stream(), self.timing.stall)))
    }

    async fn models(&self) -> Result<Vec<Model>> {
        let url = format!("{}/models", self.base);
        let response = self.send(|| self.client.get(&url)).await?;
        let text = response
            .text()
            .await
            .map_err(|e| Error::Provider(format!("the model list did not arrive: {e}")))?;
        parse_models(&text)
    }
}

fn headers(connection: &Connection, key: Option<&Secret>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(key) = key {
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| {
                Error::Config(
                "the stored key has characters a request cannot carry; enter it again in the app"
                    .into(),
            )
            })?;
        // Keeps the key out of anything that prints the headers.
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    // OpenRouter shows these beside the app's usage on the owner's account.
    if connection.base().contains("openrouter.ai") {
        headers.insert(
            "HTTP-Referer",
            HeaderValue::from_static("https://github.com/zypher-systems/tiphys"),
        );
        headers.insert("X-Title", HeaderValue::from_static("Tiphys"));
    }
    Ok(headers)
}

/// Rate limits, overload and gateway trouble: worth another attempt. A bad
/// request or a refused key will be the same the second time.
fn may_pass(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds).min(LONGEST_WAIT))
}

/// Doubles from `first`, with a little jitter so that several sessions that
/// hit one rate limit do not all come back at the same instant.
fn backoff(first: Duration, attempt: u32) -> Duration {
    let step = first.saturating_mul(1 << attempt.min(6)).min(LONGEST_WAIT);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    step + step.mul_f64(f64::from(nanos % 250) / 1000.0)
}

/// An error status, in words. The provider's own message is included; a body
/// that is not JSON is shown as it came, cut short.
fn http_error(status: reqwest::StatusCode, body: &str) -> Error {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").map(error_text))
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    let code = status.as_u16();
    let what = match code {
        401 | 403 => format!("the provider refused the key ({code})"),
        404 => format!("the provider has no such model or address ({code})"),
        402 | 429 => {
            format!("the provider is rate limiting this account or it is out of credit ({code})")
        }
        _ => format!("the provider answered {status}"),
    };
    Error::Provider(if detail.is_empty() {
        what
    } else {
        format!("{what}: {detail}")
    })
}

/// A provider's error object in its own words.
fn error_text(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or_default();
    if message.is_empty() {
        error.to_string()
    } else {
        message.to_string()
    }
}

/// The request body.
fn body(request: &Request) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        // `content` is always a string: some servers reject a null one on an
        // assistant message that only calls tools.
        let mut wire = json!({"role": role, "content": message.content});
        if let Some(id) = &message.tool_call_id {
            wire["tool_call_id"] = json!(id);
        }
        if !message.tool_calls.is_empty() {
            wire["tool_calls"] = message
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect();
        }
        messages.push(wire);
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        // Without this the token counts are left out of a streamed reply.
        "stream_options": {"include_usage": true},
    });
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
    }
    body
}

/// What one event of the stream holds.
#[derive(Debug, PartialEq)]
enum Event {
    /// `[DONE]`: the reply is whole.
    Done,
    /// Pieces of the reply. `finished` is set by the chunk that gives the
    /// reason the model stopped.
    Chunk { deltas: Vec<Delta>, finished: bool },
}

fn parse_event(data: &str) -> Result<Event> {
    if data.trim() == "[DONE]" {
        return Ok(Event::Done);
    }
    let chunk: Value = serde_json::from_str(data).map_err(|e| {
        let shown: String = data.chars().take(200).collect();
        Error::Provider(format!(
            "the provider sent something that is not JSON ({e}): {shown}"
        ))
    })?;
    // An upstream failure arrives as a chunk in a stream that started `200 OK`.
    if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
        return Err(failed_mid_reply(error));
    }
    let mut deltas = Vec::new();
    let mut finished = false;

    if let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) {
        let delta = &choice["delta"];
        if let Some(text) = text_of(delta.get("content")) {
            deltas.push(Delta::Text(text));
        }
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"));
        if let Some(text) = text_of(reasoning) {
            deltas.push(Delta::Reasoning(text));
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = &call["function"];
            let field = |v: &Value, key: &str| {
                v.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            deltas.push(Delta::ToolCall(ToolCallPart {
                slot: call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|i| u32::try_from(i).ok()),
                id: field(call, "id"),
                name: field(function, "name"),
                // Some servers send the arguments as an object, not as JSON text.
                arguments: match function.get("arguments") {
                    Some(Value::String(text)) => text.clone(),
                    Some(object @ Value::Object(_)) => object.to_string(),
                    _ => String::new(),
                },
            }));
        }
        match choice.get("finish_reason").and_then(Value::as_str) {
            None => {}
            Some("length") => {
                deltas.push(Delta::CutOff);
                finished = true;
            }
            Some("error") => {
                return Err(failed_mid_reply(
                    choice.get("error").unwrap_or(&Value::Null),
                ));
            }
            // The provider stopped the reply itself. Carrying on as if the
            // model had finished would hand back half an answer with no reason.
            Some(reason @ ("content_filter" | "refusal")) => {
                return Err(Error::Provider(format!(
                    "the provider stopped the reply: {reason}"
                )));
            }
            Some("context_length_exceeded" | "model_context_window_exceeded") => {
                return Err(Error::Provider(
                    "the conversation is longer than the model's context window".into(),
                ));
            }
            Some(_) => finished = true,
        }
    }
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
        if let Some(usage) = usage_of(usage) {
            deltas.push(Delta::Usage(usage));
        }
        let cost = usage.get("cost").or_else(|| usage.get("total_cost"));
        if let Some(cost) = cost.and_then(Value::as_f64) {
            deltas.push(Delta::Cost(cost));
        }
    }
    Ok(Event::Chunk { deltas, finished })
}

fn text_of(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn failed_mid_reply(error: &Value) -> Error {
    Error::Provider(format!(
        "the provider failed in the middle of the reply: {}",
        error_text(error)
    ))
}

fn usage_of(usage: &Value) -> Option<Usage> {
    let count = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64);
    let details = &usage["prompt_tokens_details"];
    let usage = Usage {
        input: count(usage, "prompt_tokens").unwrap_or(0),
        output: count(usage, "completion_tokens").unwrap_or(0),
        cached: count(details, "cached_tokens")
            .or_else(|| count(usage, "prompt_cache_hit_tokens"))
            .unwrap_or(0),
        cache_write: count(details, "cache_write_tokens").unwrap_or(0),
    };
    (usage != Usage::default()).then_some(usage)
}

/// Turns a response body into deltas as it arrives.
fn deltas<E: std::fmt::Display>(
    body: impl Stream<Item = std::result::Result<Bytes, E>> + Send + Unpin + 'static,
    stall: Duration,
) -> impl Stream<Item = Result<Delta>> + Send {
    struct State<S> {
        body: S,
        decoder: SseDecoder,
        ready: VecDeque<Result<Delta>>,
        /// The model gave its reason for stopping.
        finished: bool,
        /// Nothing more will be read.
        closed: bool,
        /// When the last piece of the reply arrived.
        progress: Instant,
        stall: Duration,
    }

    impl<S> State<S> {
        fn take(&mut self, data: &str) {
            match parse_event(data) {
                // Servers that keep the connection open after `[DONE]` would
                // otherwise leave a finished reply waiting for a timeout.
                Ok(Event::Done) => {
                    self.ready.push_back(Ok(Delta::Done));
                    self.closed = true;
                }
                Ok(Event::Chunk { deltas, finished }) => {
                    if !deltas.is_empty() {
                        self.progress = Instant::now();
                    }
                    self.finished |= finished;
                    self.ready.extend(deltas.into_iter().map(Ok));
                }
                Err(e) => {
                    self.ready.push_back(Err(e));
                    self.closed = true;
                }
            }
        }
    }

    let state = State {
        body,
        decoder: SseDecoder::default(),
        ready: VecDeque::new(),
        finished: false,
        closed: false,
        progress: Instant::now(),
        stall,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(next) = state.ready.pop_front() {
                return Some((next, state));
            }
            if state.closed {
                return None;
            }
            let deadline = state.progress + state.stall;
            let Ok(chunk) = tokio::time::timeout_at(deadline, state.body.next()).await else {
                state.closed = true;
                let stuck = Error::Provider(format!(
                    "nothing of the reply arrived for {} seconds; the request looks stuck at the provider",
                    state.stall.as_secs()
                ));
                return Some((Err(stuck), state));
            };
            match chunk {
                Some(Ok(bytes)) => {
                    for data in state.decoder.push(&bytes) {
                        if !state.closed {
                            state.take(&data);
                        }
                    }
                }
                Some(Err(e)) => {
                    state.closed = true;
                    let lost =
                        Error::Provider(format!("the connection was lost during the reply: {e}"));
                    return Some((Err(lost), state));
                }
                None => {
                    if let Some(data) = state.decoder.finish() {
                        state.take(&data);
                    }
                    // Some servers end a finished reply by closing, with no
                    // `[DONE]`. A body that ends before the model stopped is
                    // left without its `Done`, which the reader reports.
                    if state.finished && !state.closed {
                        state.ready.push_back(Ok(Delta::Done));
                    }
                    state.closed = true;
                }
            }
        }
    })
}

/// The deltas of a whole response body, for tests and for replaying a
/// captured stream.
pub(crate) fn deltas_of(sse: &str) -> Result<Vec<Delta>> {
    let mut decoder = SseDecoder::default();
    let mut events = decoder.push(sse.as_bytes());
    events.extend(decoder.finish());
    let mut out = Vec::new();
    let mut finished = false;
    for data in events {
        match parse_event(&data)? {
            Event::Done => {
                out.push(Delta::Done);
                return Ok(out);
            }
            Event::Chunk {
                deltas,
                finished: f,
            } => {
                out.extend(deltas);
                finished |= f;
            }
        }
    }
    if finished {
        out.push(Delta::Done);
    }
    Ok(out)
}

/// Ids that mark a model as something other than a chat model, for lists that
/// do not say what a model outputs.
const NOT_CHAT: &[&str] = &[
    "embed",
    "whisper",
    "tts",
    "dall-e",
    "image",
    "audio",
    "moderation",
    "transcribe",
    "realtime",
    "rerank",
];

/// Reads a model list, keeping the models that can hold a conversation.
fn parse_models(text: &str) -> Result<Vec<Model>> {
    let list: Value = serde_json::from_str(text)
        .map_err(|e| Error::Provider(format!("the model list is not JSON: {e}")))?;
    let rows = list
        .get("data")
        .unwrap_or(&list)
        .as_array()
        .ok_or_else(|| Error::Provider("the model list has no models in it".into()))?;
    let mut models: Vec<Model> = rows.iter().filter_map(model_of).collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

fn model_of(row: &Value) -> Option<Model> {
    let id = row.get("id").and_then(Value::as_str)?;
    let lower = id.to_ascii_lowercase();
    let outputs = row
        .get("architecture")
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array);
    let chats = match outputs {
        Some(outputs) => outputs.iter().all(|o| o.as_str() == Some("text")),
        None => !NOT_CHAT.iter().any(|word| lower.contains(word)),
    };
    if !chats {
        return None;
    }
    // Prices are listed in dollars per token, as text.
    let pricing = &row["pricing"];
    let rate = |key: &str| {
        let value = pricing.get(key)?;
        let per_token = match value {
            Value::String(text) => text.trim().parse::<f64>().ok()?,
            other => other.as_f64()?,
        };
        Some(per_token * 1_000_000.0)
    };
    let rates = rate("prompt")
        .zip(rate("completion"))
        .map(|(input, output)| Rates {
            input,
            output,
            cache_read: rate("input_cache_read"),
            cache_write: rate("input_cache_write"),
        })
        // A router's price of -1 means "it depends", which is no price.
        .filter(Rates::is_usable);
    Some(Model {
        id: id.to_string(),
        context: row
            .get("context_length")
            .or_else(|| row.get("context_window"))
            .and_then(Value::as_u64),
        rates,
        tools: row
            .get("supported_parameters")
            .and_then(Value::as_array)
            .map(|p| p.iter().any(|x| x.as_str() == Some("tools"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, ReplyBuilder, ToolCall, ToolSpec};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEXT: &str = include_str!("../../fixtures/chat/text.sse");
    const PARALLEL_TOOLS: &str = include_str!("../../fixtures/chat/parallel_tools.sse");
    const MODELS: &str = include_str!("../../fixtures/chat/models.json");

    fn reply_of(sse: &str) -> crate::llm::Reply {
        let mut builder = ReplyBuilder::default();
        for delta in deltas_of(sse).unwrap() {
            builder.push(delta);
        }
        builder.finish().unwrap()
    }

    fn chunk(json: &str) -> Result<Event> {
        parse_event(json)
    }

    fn deltas_in(json: &str) -> Vec<Delta> {
        match chunk(json).unwrap() {
            Event::Chunk { deltas, .. } => deltas,
            Event::Done => panic!("not a chunk"),
        }
    }

    #[test]
    fn a_text_reply_is_read_with_its_reasoning_and_usage() {
        let reply = reply_of(TEXT);
        assert_eq!(reply.text, "Hello world");
        assert_eq!(reply.reasoning, "think");
        assert_eq!(
            reply.usage,
            Some(Usage {
                input: 12,
                output: 3,
                cached: 2,
                cache_write: 0
            })
        );
        assert!(!reply.cut_off && reply.tool_calls.is_empty() && reply.cost.is_none());
    }

    #[test]
    fn parallel_tool_calls_keep_their_own_arguments() {
        let reply = reply_of(PARALLEL_TOOLS);
        let call = |id: &str, path: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: format!("{{\"path\":\"{path}\"}}"),
        };
        assert_eq!(
            reply.tool_calls,
            [call("call_a", "a.txt"), call("call_b", "b.txt")]
        );
        assert_eq!(reply.usage.unwrap().input, 25);
    }

    #[test]
    fn the_details_endpoints_differ_on_are_all_read() {
        // Arguments as an object, not as JSON text.
        let deltas = deltas_in(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"t","arguments":{"path":"x"}}}]}}]}"#,
        );
        assert_eq!(
            deltas,
            [Delta::ToolCall(ToolCallPart {
                slot: Some(0),
                id: "a".into(),
                name: "t".into(),
                arguments: r#"{"path":"x"}"#.into()
            })]
        );
        // Reasoning under its other name; empty and null content are nothing.
        assert_eq!(
            deltas_in(r#"{"choices":[{"delta":{"content":"","reasoning":"hm"}}]}"#),
            [Delta::Reasoning("hm".into())]
        );
        assert_eq!(deltas_in(r#"{"choices":[{"delta":{"content":null}}]}"#), []);
        // Cached tokens where one provider's own API puts them; the provider's cost.
        assert_eq!(
            deltas_in(
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":1,"prompt_cache_hit_tokens":4,"cost":0.0031}}"#
            ),
            [
                Delta::Usage(Usage {
                    input: 10,
                    output: 1,
                    cached: 4,
                    cache_write: 0
                }),
                Delta::Cost(0.0031)
            ]
        );
        // A usage of nothing is not a usage.
        assert_eq!(
            deltas_in(r#"{"choices":[],"usage":{"prompt_tokens":0}}"#),
            []
        );
        assert_eq!(deltas_in(r#"{"choices":[],"usage":null}"#), []);
    }

    #[test]
    fn the_reason_a_reply_stopped_decides_what_it_is() {
        let finish = |reason: &str| {
            chunk(&format!(
                r#"{{"choices":[{{"delta":{{}},"finish_reason":"{reason}"}}]}}"#
            ))
        };
        for whole in ["stop", "tool_calls", "end_turn"] {
            assert_eq!(
                finish(whole).unwrap(),
                Event::Chunk {
                    deltas: vec![],
                    finished: true
                },
                "{whole}"
            );
        }
        assert_eq!(
            finish("length").unwrap(),
            Event::Chunk {
                deltas: vec![Delta::CutOff],
                finished: true
            }
        );
        for stopped in [
            "content_filter",
            "refusal",
            "error",
            "context_length_exceeded",
        ] {
            assert!(
                matches!(finish(stopped), Err(Error::Provider(_))),
                "{stopped}"
            );
        }
        assert_eq!(chunk(" [DONE] ").unwrap(), Event::Done);
    }

    #[test]
    fn an_error_sent_as_a_chunk_is_an_error() {
        let err = chunk(r#"{"error":{"code":502,"message":"upstream overloaded"}}"#).unwrap_err();
        assert!(err.to_string().contains("upstream overloaded"), "{err}");
        assert!(chunk("{not json").is_err());
        // A null error is no error.
        assert!(chunk(r#"{"error":null,"choices":[]}"#).is_ok());
    }

    #[test]
    fn a_reply_that_closes_without_done_is_whole_only_if_the_model_stopped() {
        let stopped =
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n";
        assert_eq!(
            deltas_of(stopped).unwrap(),
            [Delta::Text("hi".into()), Delta::Done]
        );
        let lost = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        assert_eq!(deltas_of(lost).unwrap(), [Delta::Text("hi".into())]);
    }

    #[test]
    fn a_tool_loop_is_sent_in_the_shape_the_format_asks_for() {
        let request = Request {
            model: "vendor/model".into(),
            system: Some("You are Tiphys.".into()),
            messages: vec![
                Message::user("read a.txt"),
                Message::assistant(
                    "",
                    vec![ToolCall {
                        id: "call_a".into(),
                        name: "read_file".into(),
                        arguments: r#"{"path":"a.txt"}"#.into(),
                    }],
                ),
                Message::tool("call_a", "contents"),
            ],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "Read a file.".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            max_tokens: Some(4096),
        };
        assert_eq!(
            body(&request),
            json!({
                "model": "vendor/model",
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 4096,
                "messages": [
                    {"role": "system", "content": "You are Tiphys."},
                    {"role": "user", "content": "read a.txt"},
                    {"role": "assistant", "content": "", "tool_calls": [{
                        "id": "call_a", "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"},
                    }]},
                    {"role": "tool", "content": "contents", "tool_call_id": "call_a"},
                ],
                "tools": [{"type": "function", "function": {
                    "name": "read_file", "description": "Read a file.",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                }}],
            })
        );
        // Nothing optional is sent when there is nothing to send.
        let bare = body(&Request {
            system: None,
            tools: vec![],
            max_tokens: None,
            ..request
        });
        assert!(bare.get("tools").is_none() && bare.get("max_tokens").is_none());
        assert_eq!(bare["messages"][0]["role"], "user");
    }

    #[test]
    fn a_model_list_keeps_chat_models_with_their_prices() {
        let models = parse_models(MODELS).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["local-model", "router/auto", "vendor/free", "vendor/model"]
        );

        let priced = models.iter().find(|m| m.id == "vendor/model").unwrap();
        assert_eq!(priced.context, Some(200_000));
        assert_eq!(priced.tools, Some(true));
        let rates = priced.rates.unwrap();
        assert!((rates.input - 3.0).abs() < 1e-9 && (rates.output - 15.0).abs() < 1e-9);
        assert!((rates.cache_read.unwrap() - 0.3).abs() < 1e-9);

        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(by_id("vendor/free").rates, Some(Rates::FREE));
        // "It depends" and "not listed" are both no price.
        assert_eq!(by_id("router/auto").rates, None);
        assert_eq!(by_id("local-model").rates, None);

        assert!(parse_models("{}").is_err());
        assert!(parse_models("<html>").is_err());
        // A bare array is a list too.
        assert_eq!(parse_models(r#"[{"id":"m"}]"#).unwrap().len(), 1);
    }

    #[test]
    fn waits_between_attempts_grow_and_are_capped() {
        let first = Duration::from_millis(500);
        assert!(backoff(first, 0) >= first && backoff(first, 0) < first * 2);
        assert!(backoff(first, 2) >= first * 4);
        assert!(backoff(first, 30) <= LONGEST_WAIT + LONGEST_WAIT / 4);

        let asked = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            retry_after(&headers)
        };
        assert_eq!(asked("2"), Some(Duration::from_secs(2)));
        assert_eq!(asked("3600"), Some(LONGEST_WAIT));
        assert_eq!(asked("soon"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);

        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert!(may_pass(status), "{status}");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(!may_pass(status), "{status}");
        }
    }

    #[test]
    fn an_error_status_is_put_in_words_with_the_providers_message() {
        let status = |code: u16| reqwest::StatusCode::from_u16(code).unwrap();
        let cases = [
            (
                401,
                r#"{"error":{"message":"No auth credentials found"}}"#,
                "refused the key (401): No auth credentials found",
            ),
            (
                404,
                r#"{"error":"model not found"}"#,
                "no such model or address (404): model not found",
            ),
            (
                429,
                "",
                "rate limiting this account or it is out of credit (429)",
            ),
            (
                500,
                "<html>Bad Gateway</html>",
                "answered 500 Internal Server Error: <html>Bad Gateway</html>",
            ),
        ];
        for (code, body, expected) in cases {
            let message = http_error(status(code), body).to_string();
            assert!(message.contains(expected), "{message}");
        }
    }

    /// A server on a loopback port that answers each connection with the next
    /// response and hands back what it was sent.
    async fn serve(responses: Vec<String>) -> (Connection, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = Connection {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            model: None,
            env_key: None,
            local: false,
        };
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head, then as much body as it declares.
                loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let length = text[..head_end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + length || n == 0 {
                            break;
                        }
                    } else if n == 0 {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
            requests
        });
        (connection, server)
    }

    fn ok_stream(sse: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{sse}"
        )
    }

    fn status_response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn fast() -> Timing {
        Timing {
            backoff: Duration::from_millis(1),
            ..Timing::REMOTE
        }
    }

    fn request() -> Request {
        Request {
            model: "vendor/model".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: None,
        }
    }

    async fn collect(provider: &ChatProvider) -> Result<crate::llm::Reply> {
        let mut stream = provider.stream(request()).await?;
        let mut builder = ReplyBuilder::default();
        while let Some(delta) = stream.next().await {
            builder.push(delta?);
        }
        builder.finish()
    }

    #[tokio::test]
    async fn a_reply_streams_over_http_and_the_key_goes_in_the_header() {
        let (connection, server) = serve(vec![ok_stream(TEXT)]).await;
        let key = Secret::new("sk-test-123").unwrap();
        let provider = ChatProvider::with_timing(&connection, Some(&key), fast()).unwrap();

        let reply = collect(&provider).await.unwrap();
        assert_eq!(reply.text, "Hello world");

        let sent = server.await.unwrap().remove(0);
        assert!(
            sent.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{sent}"
        );
        assert!(
            sent.to_ascii_lowercase()
                .contains("authorization: bearer sk-test-123")
        );
        assert!(sent.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn a_rate_limit_is_waited_out_and_tried_again() {
        let (connection, server) = serve(vec![
            status_response(
                "429 Too Many Requests",
                "retry-after: 0\r\n",
                r#"{"error":{"message":"slow down"}}"#,
            ),
            status_response("503 Service Unavailable", "", ""),
            ok_stream(TEXT),
        ])
        .await;
        let provider = ChatProvider::with_timing(&connection, None, fast()).unwrap();
        assert_eq!(collect(&provider).await.unwrap().text, "Hello world");
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_refused_key_fails_at_once_and_the_key_is_not_in_the_error() {
        let (connection, server) = serve(vec![status_response(
            "401 Unauthorized",
            "",
            r#"{"error":{"message":"Invalid API key"}}"#,
        )])
        .await;
        let key = Secret::new("sk-test-123").unwrap();
        let provider = ChatProvider::with_timing(&connection, Some(&key), fast()).unwrap();

        let err = collect(&provider).await.unwrap_err().to_string();
        assert!(
            err.contains("refused the key (401): Invalid API key"),
            "{err}"
        );
        assert!(!err.contains("sk-test-123"));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retries_run_out_and_the_last_answer_is_the_error() {
        let busy = || {
            status_response(
                "503 Service Unavailable",
                "",
                r#"{"error":{"message":"overloaded"}}"#,
            )
        };
        let (connection, server) = serve(vec![busy(), busy()]).await;
        let timing = Timing {
            retries: 1,
            ..fast()
        };
        let provider = ChatProvider::with_timing(&connection, None, timing).unwrap();

        let err = collect(&provider).await.unwrap_err().to_string();
        assert!(err.contains("503") && err.contains("overloaded"), "{err}");
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_local_server_that_is_not_running_says_so_without_retrying() {
        // Bind and drop, so the port is known to be closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = Connection {
            base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
            model: None,
            env_key: None,
            local: true,
        };
        drop(listener);
        let provider = ChatProvider::new(&connection, None).unwrap();
        let err = collect(&provider).await.unwrap_err().to_string();
        assert!(err.contains("is it running?"), "{err}");
    }

    #[tokio::test]
    async fn the_model_list_is_fetched_from_the_connection() {
        let (connection, server) = serve(vec![status_response("200 OK", "", MODELS)]).await;
        let provider = ChatProvider::with_timing(&connection, None, fast()).unwrap();
        let models = provider.models().await.unwrap();
        assert_eq!(models.len(), 4);
        assert!(server.await.unwrap()[0].starts_with("GET /v1/models HTTP/1.1"));
    }

    type Chunk = std::result::Result<Bytes, std::io::Error>;

    async fn all(stream: impl Stream<Item = Result<Delta>>) -> Vec<Result<Delta>> {
        stream.collect().await
    }

    #[tokio::test(start_paused = true)]
    async fn keep_alives_alone_do_not_hold_a_reply_open_for_ever() {
        // A provider that sends a comment every few seconds and never a delta.
        let keep_alives = futures_util::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Some((Chunk::Ok(Bytes::from_static(b": processing\n\n")), ()))
        });
        let got = all(deltas(Box::pin(keep_alives), Duration::from_secs(60))).await;
        assert_eq!(got.len(), 1);
        let err = got[0].as_ref().unwrap_err().to_string();
        assert!(err.contains("60 seconds"), "{err}");
    }

    #[tokio::test]
    async fn a_finished_reply_ends_without_waiting_for_the_connection_to_close() {
        // `[DONE]`, then a connection that stays open.
        let body = futures_util::stream::iter(vec![Chunk::Ok(Bytes::from(TEXT))])
            .chain(futures_util::stream::pending());
        let got = all(deltas(Box::pin(body), Duration::from_secs(3600))).await;
        assert_eq!(got.last().unwrap().as_ref().unwrap(), &Delta::Done);
    }

    #[tokio::test]
    async fn a_lost_connection_or_an_error_chunk_ends_the_stream_with_an_error() {
        let lost = futures_util::stream::iter(vec![
            Chunk::Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            )),
            Chunk::Err(std::io::Error::other("reset by peer")),
            Chunk::Ok(Bytes::from_static(b"data: [DONE]\n\n")),
        ]);
        let got = all(deltas(lost, Duration::from_secs(60))).await;
        assert_eq!(got.len(), 2);
        assert!(
            got[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("reset by peer")
        );

        let failed = futures_util::stream::iter(vec![Chunk::Ok(Bytes::from_static(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"error\":{\"message\":\"boom\"}}\n\ndata: [DONE]\n\n",
        ))]);
        let got = all(deltas(failed, Duration::from_secs(60))).await;
        assert_eq!(got.len(), 2);
        assert!(got[1].as_ref().unwrap_err().to_string().contains("boom"));
    }
}
