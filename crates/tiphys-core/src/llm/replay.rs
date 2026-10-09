//! A provider that plays back replies written in advance.
//!
//! Tests of the agent loop and of the app run against this instead of a
//! network. Each call to `stream` returns the next scripted reply, and every
//! request is kept so a test can check what the model was actually sent.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{Delta, DeltaStream, Model, Provider, Request, chat};
use crate::{Error, Result};

/// Plays scripted replies in order.
#[derive(Debug, Default)]
pub struct ReplayProvider {
    replies: Mutex<VecDeque<Result<Vec<Delta>>>>,
    requests: Mutex<Vec<Request>>,
    models: Vec<Model>,
}

impl ReplayProvider {
    /// A provider that gives these replies, one per call.
    pub fn new(replies: Vec<Vec<Delta>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(Ok).collect()),
            ..Self::default()
        }
    }

    /// A provider whose one reply is a Chat Completions response body.
    pub fn from_chat_sse(sse: &str) -> Result<Self> {
        Ok(Self::new(vec![chat::deltas_of(sse)?]))
    }

    /// Adds a reply after the ones already scripted.
    pub fn then(self, reply: Vec<Delta>) -> Self {
        self.replies.lock().unwrap().push_back(Ok(reply));
        self
    }

    /// Adds a call that fails before anything is streamed.
    pub fn then_fail(self, message: &str) -> Self {
        let failure = Err(Error::Provider(message.to_string()));
        self.replies.lock().unwrap().push_back(failure);
        self
    }

    /// Sets the model list this provider reports.
    pub fn with_models(mut self, models: Vec<Model>) -> Self {
        self.models = models;
        self
    }

    /// Every request made so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    async fn stream(&self, request: Request) -> Result<DeltaStream> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(Error::Provider(
                    "replay: no reply left in the script".into(),
                ))
            })?;
        Ok(Box::pin(futures_util::stream::iter(
            reply.into_iter().map(Ok),
        )))
    }

    async fn models(&self) -> Result<Vec<Model>> {
        Ok(self.models.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, ReplyBuilder};
    use futures_util::StreamExt;

    fn request(text: &str) -> Request {
        Request {
            model: "m".into(),
            system: None,
            messages: vec![Message::user(text)],
            tools: vec![],
            max_tokens: None,
        }
    }

    async fn text_of(provider: &ReplayProvider, ask: &str) -> Result<String> {
        let mut stream = provider.stream(request(ask)).await?;
        let mut builder = ReplyBuilder::default();
        while let Some(delta) = stream.next().await {
            builder.push(delta?);
        }
        Ok(builder.finish()?.text)
    }

    #[tokio::test]
    async fn replies_play_in_order_and_requests_are_kept() {
        let provider = ReplayProvider::new(vec![vec![Delta::Text("one".into()), Delta::Done]])
            .then(vec![Delta::Text("two".into()), Delta::Done])
            .then_fail("rate limited");

        assert_eq!(text_of(&provider, "first").await.unwrap(), "one");
        assert_eq!(text_of(&provider, "second").await.unwrap(), "two");
        assert!(
            text_of(&provider, "third")
                .await
                .unwrap_err()
                .to_string()
                .contains("rate limited")
        );
        assert!(text_of(&provider, "fourth").await.is_err());

        let asked: Vec<String> = provider
            .requests()
            .iter()
            .map(|r| r.messages[0].content.clone())
            .collect();
        assert_eq!(asked, ["first", "second", "third", "fourth"]);
    }

    #[tokio::test]
    async fn a_response_body_plays_back_through_the_real_parser() {
        let sse = include_str!("../../fixtures/chat/text.sse");
        let provider = ReplayProvider::from_chat_sse(sse).unwrap();
        assert_eq!(text_of(&provider, "hi").await.unwrap(), "Hello world");
    }
}
