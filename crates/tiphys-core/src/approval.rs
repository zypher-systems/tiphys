//! Asking the owner before an action runs.
//!
//! An action that reaches beyond the agent's own home is put to an
//! [`Approver`]. What answers depends on where the turn is running: in the
//! app the owner is shown a card and presses a key; in a one-shot run nobody
//! is there, and nobody answering is a no.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::policy::Class;

/// How long the owner has to answer before the answer is no.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(300);

/// An action waiting for a yes or a no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// Identifies the question to whoever answers it.
    pub id: String,
    pub tool: String,
    /// What the action will do, in a line.
    pub summary: String,
    /// Why, in the model's words.
    pub reason: String,
    /// Why it needs asking, in the rules' words.
    pub why: String,
    pub class: Class,
    /// What will change, such as a diff, when there is something to show.
    pub preview: Option<String>,
}

/// The answer to an [`Ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approve,
    /// No, with what the model is told about it.
    Deny(String),
}

/// Whoever can say yes to an action.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, ask: &Ask) -> Decision;
}

/// Nobody is there to ask, so the answer is always no.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn decide(&self, _: &Ask) -> Decision {
        Decision::Deny("nobody is here to approve it".into())
    }
}

/// Questions waiting on an answer from a client. The question itself reaches
/// the client as an event; this holds the turn until [`Pending::answer`] is
/// called for it, or until it has waited too long.
#[derive(Debug)]
pub struct Pending {
    waiting: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
    within: Duration,
}

impl Pending {
    pub fn new(within: Duration) -> Self {
        Self {
            waiting: Mutex::default(),
            within,
        }
    }

    /// Answers the question `id`. Returns whether it was still waiting; an
    /// answer to a question that is gone changes nothing.
    pub fn answer(&self, id: &str, decision: Decision) -> bool {
        let waiting = self.waiting.lock().unwrap().remove(id);
        waiting.is_some_and(|waiting| waiting.send(decision).is_ok())
    }
}

impl Default for Pending {
    fn default() -> Self {
        Self::new(ANSWER_WITHIN)
    }
}

#[async_trait]
impl Approver for Pending {
    async fn decide(&self, ask: &Ask) -> Decision {
        let (answer, answered) = oneshot::channel();
        self.waiting.lock().unwrap().insert(ask.id.clone(), answer);
        let decision = match tokio::time::timeout(self.within, answered).await {
            Ok(Ok(decision)) => decision,
            // Dropped without an answer: whoever was asked has gone away.
            Ok(Err(_)) => Decision::Deny("nobody was left to answer".into()),
            Err(_) => Decision::Deny(format!(
                "nobody answered within {} minutes",
                self.within.as_secs().div_ceil(60)
            )),
        };
        self.waiting.lock().unwrap().remove(&ask.id);
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ask(id: &str) -> Ask {
        Ask {
            id: id.into(),
            tool: "write_file".into(),
            summary: "write /etc/hosts".into(),
            reason: "to add a host".into(),
            why: "/etc/hosts is outside Tiphys's own home".into(),
            class: Class::System,
            preview: None,
        }
    }

    #[tokio::test]
    async fn with_nobody_to_ask_the_answer_is_no() {
        let Decision::Deny(note) = DenyAll.decide(&ask("a")).await else {
            panic!("approved with nobody there");
        };
        assert!(note.contains("nobody is here"));
    }

    #[tokio::test]
    async fn a_question_waits_for_its_own_answer() {
        let pending = Arc::new(Pending::default());
        let asking = {
            let pending = pending.clone();
            tokio::spawn(async move { pending.decide(&ask("a")).await })
        };
        // Let the question be registered.
        while pending.waiting.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        // An answer to some other question does not reach this one.
        assert!(!pending.answer("b", Decision::Approve));
        assert!(pending.answer("a", Decision::Approve));
        assert_eq!(asking.await.unwrap(), Decision::Approve);
        // Answered once; a second answer finds nothing waiting.
        assert!(!pending.answer("a", Decision::Deny("late".into())));
    }

    #[tokio::test(start_paused = true)]
    async fn a_question_nobody_answers_becomes_a_no() {
        let pending = Pending::default();
        let Decision::Deny(note) = pending.decide(&ask("a")).await else {
            panic!("approved by silence");
        };
        assert_eq!(note, "nobody answered within 5 minutes");
        assert!(pending.waiting.lock().unwrap().is_empty());
    }
}
