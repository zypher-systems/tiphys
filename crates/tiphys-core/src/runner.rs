//! Where the agent's actions are planned and run.
//!
//! The agent loop does not touch the machine itself. It hands each tool call
//! to a [`Runner`], which says what the call would do and what the rules make
//! of it, and then, if the loop lets it, does it.
//!
//! There are two runners. [`LocalRunner`] acts in this process, as the user
//! Tiphys was started by. [`crate::worker::WorkerRunner`] hands every call to
//! a worker process running as another user, one that cannot read the key
//! store or the state directory; that is how an installed Tiphys runs.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::llm::ToolCall;
use crate::policy::Verdict;
use crate::prompt::Machine;
use crate::tools::{Action, Output, Registry, ToolCtx};

/// A call that has been planned and is waiting to be judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Names this plan to the runner that made it.
    pub ticket: u64,
    pub verdict: Verdict,
    /// What it will do, in a line.
    pub summary: String,
    /// What will change, when there is something to show.
    pub preview: Option<String>,
    /// Why, in the model's words.
    pub reason: String,
}

/// Something that plans and runs tool calls.
#[async_trait]
pub trait Runner: Send + Sync {
    /// The machine and the user the actions run as.
    fn machine(&self) -> Machine;

    /// Plans a call. An error is a message for the model.
    async fn plan(&self, call: &ToolCall) -> Result<Planned, String>;

    /// Runs a planned call. Dropping the future this returns stops the
    /// action, with everything it started.
    async fn run(&self, ticket: u64) -> Output;

    /// Forgets a planned call that will not be run.
    fn discard(&self, ticket: u64);

    /// Whether it can still act. A worker process can die; when it has, the
    /// session needs a new runner before its next turn.
    fn is_alive(&self) -> bool {
        true
    }
}

/// Plans and runs in this process.
pub struct LocalRunner {
    registry: Registry,
    ctx: ToolCtx,
    machine: Machine,
    planned: Mutex<HashMap<u64, Box<dyn Action>>>,
    next: AtomicU64,
}

impl LocalRunner {
    pub fn new(registry: Registry, ctx: ToolCtx) -> Self {
        Self {
            machine: Machine::detect(&ctx.home),
            registry,
            ctx,
            planned: Mutex::default(),
            next: AtomicU64::new(1),
        }
    }
}

#[async_trait]
impl Runner for LocalRunner {
    fn machine(&self) -> Machine {
        self.machine.clone()
    }

    async fn plan(&self, call: &ToolCall) -> Result<Planned, String> {
        let planned = self.registry.plan(call, &self.ctx)?;
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        let plan = Planned {
            ticket,
            verdict: planned.action.verdict(),
            summary: planned.action.summary(),
            preview: planned.action.preview(),
            reason: planned.reason,
        };
        self.planned.lock().unwrap().insert(ticket, planned.action);
        Ok(plan)
    }

    async fn run(&self, ticket: u64) -> Output {
        let action = self.planned.lock().unwrap().remove(&ticket);
        match action {
            Some(action) => action.run(&self.ctx).await,
            None => Output::error("there is no such planned action"),
        }
    }

    fn discard(&self, ticket: u64) {
        self.planned.lock().unwrap().remove(&ticket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Class;

    fn runner() -> (tempfile::TempDir, LocalRunner) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".tiphys/keys")).unwrap();
        std::fs::write(home.join("hello.txt"), "hello\n").unwrap();
        let ctx = ToolCtx {
            state: home.join(".tiphys"),
            home: home.clone(),
            cwd: home,
        };
        (dir, LocalRunner::new(Registry::builtin(), ctx))
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[tokio::test]
    async fn a_call_is_planned_then_run_by_its_ticket() {
        let (_dir, runner) = runner();
        let planned = runner
            .plan(&call(
                "read_file",
                r#"{"path":"hello.txt","reason":"to see it"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(planned.verdict.class, Class::Observe);
        assert!(planned.summary.starts_with("read ") && planned.summary.ends_with("hello.txt"));
        assert_eq!(planned.reason, "to see it");
        assert_eq!(runner.run(planned.ticket).await, Output::ok("hello\n"));
        // A ticket is good once.
        assert!(!runner.run(planned.ticket).await.ok);
    }

    #[tokio::test]
    async fn a_discarded_plan_cannot_be_run_and_a_bad_call_is_explained() {
        let (_dir, runner) = runner();
        let planned = runner
            .plan(&call("write_file", r#"{"path":"new.txt","content":"x"}"#))
            .await
            .unwrap();
        assert!(planned.preview.as_ref().unwrap().contains("+x"));
        runner.discard(planned.ticket);
        assert!(!runner.run(planned.ticket).await.ok);

        let message = runner.plan(&call("launch_rocket", "{}")).await.unwrap_err();
        assert!(message.contains("no tool named"), "{message}");
        let refused = runner
            .plan(&call("read_file", r#"{"path":"~/.tiphys/keys/work"}"#))
            .await
            .unwrap();
        assert_eq!(refused.verdict.class, Class::Never);
    }
}
