//! The tools the model can call.
//!
//! A [`Tool`] does not act. It turns the model's arguments into an
//! [`Action`]: something with a class, a one-line summary, and a way to run.
//! The agent loop looks at the class, decides whether the action may go
//! ahead, and only then runs it. A tool has no way around that.
//!
//! Every tool is told to give a `reason`, a line on why it is being called.
//! The registry adds that argument to each tool's schema and takes it back
//! out, so a tool never sees it and cannot forget it.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::llm::{ToolCall, ToolSpec};
use crate::policy::Verdict;
use crate::policy::paths::Places;

pub mod fs;
pub mod shell;
pub mod write;

/// The most a tool's output may be, in bytes. A model that is handed a
/// megabyte of log has less room left to think about it.
pub const OUTPUT_LIMIT: usize = 32_000;

/// Where a tool is running.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// The state directory.
    pub state: PathBuf,
    /// The home of the user Tiphys runs as. `~` means this.
    pub home: PathBuf,
    /// Where a relative path starts from.
    pub cwd: PathBuf,
}

impl ToolCtx {
    /// The places the path rules are about.
    pub fn places(&self) -> Places<'_> {
        Places {
            state: &self.state,
            home: &self.home,
        }
    }

    /// Where a path the model gave points: `~` is the Tiphys user's home, and
    /// a relative path starts from the working directory.
    pub fn locate(&self, path: &str) -> PathBuf {
        let path = path.trim();
        if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            self.cwd.join(path)
        }
    }
}

/// What a tool handed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub text: String,
    /// Whether it did what was asked. A failure still goes to the model, as
    /// text it can read and act on.
    pub ok: bool,
}

impl Output {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: true,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: false,
        }
    }

    /// Cuts the text to [`OUTPUT_LIMIT`] and says how much was left out.
    pub fn capped(mut self) -> Self {
        if self.text.len() > OUTPUT_LIMIT {
            let mut cut = OUTPUT_LIMIT;
            while !self.text.is_char_boundary(cut) {
                cut -= 1;
            }
            let left_out = self.text.len() - cut;
            self.text.truncate(cut);
            self.text
                .push_str(&format!("\n[cut here: {left_out} more bytes not shown]"));
        }
        self
    }
}

/// Something the model can call.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    /// What it does and when to use it, for the model.
    fn description(&self) -> &'static str;

    /// A JSON Schema object for its arguments, without `reason`.
    fn parameters(&self) -> Value;

    /// Turns arguments into an action, or says what is wrong with them. The
    /// message goes back to the model.
    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String>;
}

/// One thing a tool is about to do.
#[async_trait]
pub trait Action: Send {
    /// What the rules say about it.
    fn verdict(&self) -> Verdict;

    /// What it will do, in a line the owner can read.
    fn summary(&self) -> String;

    /// What will change, for the owner to look at before saying yes: a diff,
    /// or the command in full.
    fn preview(&self) -> Option<String> {
        None
    }

    async fn run(self: Box<Self>, ctx: &ToolCtx) -> Output;
}

/// An action ready to be judged, with the model's reason for it.
pub struct Planned {
    pub action: Box<dyn Action>,
    pub reason: String,
}

/// The tools a session has, in the order the model is told about them.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// The tools every session starts with.
    pub fn builtin() -> Self {
        Self::default()
            .with(fs::ReadFile)
            .with(fs::ListDir)
            .with(fs::SearchFiles)
            .with(write::WriteFile)
            .with(write::EditFile)
            .with(shell::Shell)
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// The tools as the model is told about them, each with its `reason`.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|tool| {
                let mut parameters = tool.parameters();
                parameters["properties"]["reason"] = json!({
                    "type": "string",
                    "description": "One line on why you are doing this. The owner reads it.",
                });
                match parameters.get_mut("required").and_then(Value::as_array_mut) {
                    Some(required) => required.push(json!("reason")),
                    None => parameters["required"] = json!(["reason"]),
                }
                ToolSpec {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters,
                }
            })
            .collect()
    }

    /// Plans a call: finds the tool, reads the arguments, and asks the tool
    /// what it would do. An error is a message for the model.
    pub fn plan(&self, call: &ToolCall, ctx: &ToolCtx) -> Result<Planned, String> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .ok_or_else(|| format!("there is no tool named `{}`", call.name))?;
        let mut args = call.args()?;
        let Some(object) = args.as_object_mut() else {
            return Err("the arguments must be a JSON object".into());
        };
        let reason = match object.remove("reason") {
            Some(Value::String(reason)) => reason.trim().to_string(),
            _ => String::new(),
        };
        let action = tool.plan(args, ctx)?;
        Ok(Planned { action, reason })
    }
}

/// Reads a tool's arguments into its own type, with an error the model can
/// correct itself from.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    struct Say(String);

    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "Says it back."
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
        }
        fn plan(&self, args: Value, _: &ToolCtx) -> Result<Box<dyn Action>, String> {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                text: String,
            }
            let Args { text } = parse_args(args)?;
            Ok(Box::new(Say(text)))
        }
    }

    #[async_trait]
    impl Action for Say {
        fn verdict(&self) -> Verdict {
            Verdict::observe()
        }
        fn summary(&self) -> String {
            format!("say {}", self.0)
        }
        async fn run(self: Box<Self>, _: &ToolCtx) -> Output {
            Output::ok(self.0)
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            state: "/state".into(),
            home: "/home/tiphys".into(),
            cwd: "/home/tiphys".into(),
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn every_tool_is_told_to_give_a_reason() {
        let specs = Registry::default().with(Echo).specs();
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].parameters["required"], json!(["text", "reason"]));
        assert_eq!(
            specs[0].parameters["properties"]["reason"]["type"],
            "string"
        );
        // The built-in tools all end up with one as well.
        for spec in Registry::builtin().specs() {
            let required = spec.parameters["required"].as_array().unwrap();
            assert!(required.contains(&json!("reason")), "{}", spec.name);
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
        }
    }

    #[tokio::test]
    async fn a_call_is_planned_with_its_reason_taken_out() {
        let registry = Registry::default().with(Echo);
        let planned = registry
            .plan(
                &call("echo", r#"{"text":"hi","reason":" to test "}"#),
                &ctx(),
            )
            .unwrap();
        assert_eq!(planned.reason, "to test");
        assert_eq!(planned.action.summary(), "say hi");
        assert_eq!(planned.action.run(&ctx()).await, Output::ok("hi"));
    }

    #[test]
    fn a_call_that_cannot_be_planned_says_why_in_words_for_the_model() {
        let registry = Registry::default().with(Echo);
        let cases = [
            (call("nope", "{}"), "no tool named `nope`"),
            (call("echo", "{\"text\":"), "not JSON"),
            (call("echo", "[1]"), "must be a JSON object"),
            (call("echo", "{}"), "bad arguments: missing field `text`"),
            (
                call("echo", r#"{"text":"a","extra":1}"#),
                "unknown field `extra`",
            ),
        ];
        for (call, expected) in cases {
            let Err(message) = registry.plan(&call, &ctx()) else {
                panic!("{} planned", call.arguments);
            };
            assert!(message.contains(expected), "{message}");
        }
        // A missing reason does not stop a call; it is just empty.
        assert_eq!(
            registry
                .plan(&call("echo", r#"{"text":"a"}"#), &ctx())
                .unwrap()
                .reason,
            ""
        );
    }

    #[test]
    fn long_output_is_cut_on_a_character_and_says_so() {
        let short = Output::ok("fine").capped();
        assert_eq!(short.text, "fine");

        let long = Output::ok("é".repeat(OUTPUT_LIMIT)).capped();
        assert!(long.text.len() < OUTPUT_LIMIT + 60);
        assert!(long.text.ends_with("more bytes not shown]"));
        assert!(long.ok);
    }
}
