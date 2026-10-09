//! The agent loop.
//!
//! A turn starts with a message and runs in rounds. Each round sends the
//! conversation to the model and reads its reply; if the reply calls tools,
//! each call is planned, judged by its class, run, and answered, and the next
//! round begins. The turn ends when the model answers without calling
//! anything, or when one of the guards stops it: the round limit, the same
//! call repeating, a reply that keeps being cut off, a cancel, or a failure.
//!
//! Whatever happens, the transcript is left in a state a provider accepts:
//! every call the model made has a result after it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::Utc;
use futures_util::StreamExt;

use crate::Result;
use crate::cancel::Cancel;
use crate::config::Limits;
use crate::llm::{self, Delta, Message, Provider, Reply, ReplyBuilder, ToolCall};
use crate::policy::Class;
use crate::proto::{Event, StopReason};
use crate::session::Session;
use crate::spend::{self, Charge, PriceBook};
use crate::tools::{Output, Registry, ToolCtx};

/// The same call with the same result this many times gets a note.
const REPEAT_NUDGE: u32 = 3;
/// And this many times ends the turn.
const REPEAT_STOP: u32 = 5;
/// Replies cut off at the output limit in a row before the turn ends.
const MAX_CUT_OFFS: u32 = 3;
/// How much of a tool's output an event carries, in characters.
const EVENT_OUTPUT_CHARS: usize = 2000;

const REPEAT_NOTE: &str = "\n\n[Tiphys: you have made this exact call several times and it \
returns the same thing. Calling it again will not change that. Use what you have, or try \
something different.]";
const CUT_OFF_NOTE: &str = "[Tiphys: your last reply was cut off at the output limit, and any \
tool call in it was dropped. Carry on from where you stopped, in smaller steps.]";
const CANCELLED_RESULT: &str = "not run: the owner stopped the turn";
const INTERRUPTED_RESULT: &str = "not run: Tiphys stopped before this call was dealt with";

/// What a turn came to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Turn {
    pub reason: StopReason,
    /// The last thing the model said.
    pub text: String,
    pub rounds: u32,
    pub tool_calls: u32,
    /// What went wrong, when the reason is [`StopReason::Failed`].
    pub error: Option<String>,
}

/// Where a turn's events go.
pub type Emit<'a> = &'a (dyn Fn(&Event) + Send + Sync);

/// One session's agent.
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub session: Session,
    pub tools: Registry,
    pub ctx: ToolCtx,
    pub prices: PriceBook,
    /// Whether the connection is on the owner's own hardware, and so free.
    pub local: bool,
    pub limits: Limits,
    pub cancel: Arc<Cancel>,
}

impl Agent {
    /// Runs one turn to its end and says how it ended. Events go to `emit` as
    /// they happen; the durable ones are also written to the session.
    pub async fn turn(&mut self, input: &str, emit: Emit<'_>) -> Turn {
        let mut turn = Turn::default();
        turn.reason = match self.run(input, emit, &mut turn).await {
            Ok(reason) => reason,
            Err(e) => {
                turn.error = Some(e.to_string());
                StopReason::Failed
            }
        };
        // If the disk is what failed, these may fail too. The caller still
        // gets the turn and its error.
        let _ = self.session.answer_unanswered(INTERRUPTED_RESULT);
        let _ = self.tell(
            emit,
            Event::TurnFinished {
                reason: turn.reason,
                error: turn.error.clone(),
            },
        );
        turn
    }

    async fn run(&mut self, input: &str, emit: Emit<'_>, turn: &mut Turn) -> Result<StopReason> {
        self.cancel.reset();
        // A turn that died part-way may have left calls without results.
        self.session.answer_unanswered(INTERRUPTED_RESULT)?;
        self.session.push(Message::user(input))?;
        self.tell(
            emit,
            Event::UserMessage {
                text: input.to_string(),
            },
        )?;

        let mut repeats: HashMap<u64, u32> = HashMap::new();
        let mut cut_offs = 0;
        for _ in 0..self.limits.rounds {
            if self.cancel.is_cancelled() {
                return Ok(StopReason::Cancelled);
            }
            turn.rounds += 1;
            let Some(reply) = self.ask(emit).await? else {
                return Ok(StopReason::Cancelled);
            };
            self.charge(&reply, emit)?;

            if reply.cut_off {
                // The text is kept; the calls are not, since the last of them
                // may be half-written.
                if !reply.text.is_empty() {
                    self.say(emit, &reply.text, Vec::new(), turn)?;
                }
                cut_offs += 1;
                if cut_offs == MAX_CUT_OFFS {
                    return Ok(StopReason::CutOff);
                }
                self.session.push(Message::user(CUT_OFF_NOTE))?;
                self.tell(emit, Event::Notice { text: "The reply was cut off at the output limit; asking the model to carry on.".into() })?;
                continue;
            }
            cut_offs = 0;

            let calls = reply.tool_calls;
            self.say(emit, &reply.text, calls.clone(), turn)?;
            if calls.is_empty() {
                return Ok(StopReason::Completed);
            }

            let mut stuck = false;
            let mut cancelled = false;
            for call in &calls {
                cancelled |= self.cancel.is_cancelled();
                if cancelled {
                    self.session
                        .push(Message::tool(&call.id, CANCELLED_RESULT))?;
                    continue;
                }
                let output = self.deal_with(call, emit).await?;
                turn.tool_calls += 1;
                let seen = repeats.entry(fingerprint(call, &output)).or_insert(0);
                *seen += 1;
                let mut text = output.text;
                if *seen == REPEAT_NUDGE {
                    text.push_str(REPEAT_NOTE);
                }
                stuck |= *seen >= REPEAT_STOP;
                self.session.push(Message::tool(&call.id, text))?;
            }
            if cancelled || self.cancel.is_cancelled() {
                return Ok(StopReason::Cancelled);
            }
            if stuck {
                return Ok(StopReason::Stuck);
            }
        }
        self.tell(
            emit,
            Event::Notice {
                text: format!(
                    "Stopped at the limit of {} rounds for one turn. Say \"continue\" to go on.",
                    self.limits.rounds
                ),
            },
        )?;
        Ok(StopReason::Rounds)
    }

    /// Sends the conversation and reads the reply, passing its text on as it
    /// streams. `None` means the owner cancelled while it was arriving, and
    /// nothing of it is kept.
    async fn ask(&mut self, emit: Emit<'_>) -> Result<Option<Reply>> {
        let request = llm::Request {
            model: self.session.meta().model.clone(),
            system: Some(self.session.system().to_string()),
            messages: self.session.messages().to_vec(),
            tools: self.session.tools().to_vec(),
            max_tokens: self.limits.max_tokens,
        };
        let cancel = self.cancel.clone();
        let mut stream = tokio::select! {
            stream = self.provider.stream(request) => stream?,
            () = cancel.cancelled() => return Ok(None),
        };
        let mut reply = ReplyBuilder::default();
        loop {
            let delta = tokio::select! {
                delta = stream.next() => delta,
                () = cancel.cancelled() => return Ok(None),
            };
            let Some(delta) = delta else {
                break;
            };
            let delta = delta?;
            match &delta {
                Delta::Text(text) => emit(&Event::Text { text: text.clone() }),
                Delta::Reasoning(text) => emit(&Event::Reasoning { text: text.clone() }),
                _ => {}
            }
            reply.push(delta);
        }
        reply.finish().map(Some)
    }

    /// Writes what the call cost to the ledger and reports it.
    fn charge(&mut self, reply: &Reply, emit: Emit<'_>) -> Result<()> {
        let meta = self.session.meta();
        let rates = self.prices.rates(&meta.connection, &meta.model, self.local);
        let cost = spend::cost_of(reply.cost, rates, reply.usage.as_ref());
        let charge = Charge {
            at: Utc::now(),
            session: meta.id.clone(),
            connection: meta.connection.clone(),
            model: meta.model.clone(),
            usage: reply.usage,
            cost,
        };
        spend::record(&self.ctx.state, &charge)?;
        self.tell(
            emit,
            Event::Spend {
                cost,
                usage: reply.usage,
            },
        )
    }

    /// Adds the model's message to the transcript and reports its text.
    fn say(
        &mut self,
        emit: Emit<'_>,
        text: &str,
        calls: Vec<ToolCall>,
        turn: &mut Turn,
    ) -> Result<()> {
        self.session.push(Message::assistant(text, calls))?;
        if !text.is_empty() {
            turn.text = text.to_string();
            self.tell(
                emit,
                Event::AssistantMessage {
                    text: text.to_string(),
                },
            )?;
        }
        Ok(())
    }

    /// Plans one call, decides whether it may run, runs it, and returns what
    /// goes back to the model. A call that cannot run is not an error of the
    /// turn: the model is told why and carries on.
    async fn deal_with(&mut self, call: &ToolCall, emit: Emit<'_>) -> Result<Output> {
        let planned = self.tools.plan(call, &self.ctx);
        let (summary, reason) = match &planned {
            Ok(planned) => (planned.action.summary(), planned.reason.clone()),
            Err(_) => (call.name.clone(), String::new()),
        };
        self.tell(
            emit,
            Event::ToolStarted {
                id: call.id.clone(),
                tool: call.name.clone(),
                summary,
                reason,
            },
        )?;
        let output = match planned {
            Err(message) => Output::error(message),
            Ok(planned) => match planned.action.class() {
                Class::Observe => {
                    let cancel = self.cancel.clone();
                    tokio::select! {
                        output = planned.action.run(&self.ctx) => output,
                        () = cancel.cancelled() => Output::error("stopped by the owner before it finished"),
                    }
                }
                Class::Never => Output::error(
                    "refused: this is something Tiphys never does, whoever asks. Do not try another way.",
                ),
                Class::Change | Class::System => Output::error(
                    "not run: this needs the owner's approval, and this version of Tiphys cannot ask for it yet",
                ),
            },
        }
        .capped();
        self.tell(
            emit,
            Event::ToolFinished {
                id: call.id.clone(),
                ok: output.ok,
                output: output.text.chars().take(EVENT_OUTPUT_CHARS).collect(),
            },
        )?;
        Ok(output)
    }

    /// Reports an event: to the session's event file if it is durable, then
    /// to whoever is watching.
    fn tell(&mut self, emit: Emit<'_>, event: Event) -> Result<()> {
        self.session.log(&event)?;
        emit(&event);
        Ok(())
    }
}

/// Identifies a call together with what it returned, to notice a loop.
fn fingerprint(call: &ToolCall, output: &Output) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&call.name, &call.arguments, &output.text).hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{DeltaStream, Model, ReplayProvider, Role, ToolCallPart};
    use crate::session::Opening;
    use crate::spend::{Rates, Usage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct Fixture {
        _dir: tempfile::TempDir,
        agent: Agent,
        provider: Arc<ReplayProvider>,
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl Fixture {
        async fn turn(&mut self, input: &str) -> Turn {
            let events = self.events.clone();
            self.agent
                .turn(input, &move |event| {
                    events.lock().unwrap().push(event.clone())
                })
                .await
        }

        /// The kinds of the durable events so far, in order.
        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.is_durable())
                .map(|event| {
                    serde_json::to_value(event).unwrap()["kind"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        }

        fn home(&self) -> std::path::PathBuf {
            self.agent.ctx.home.clone()
        }
    }

    fn fixture_with(provider: Arc<dyn Provider>, replay: Arc<ReplayProvider>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = home.join(".tiphys");
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::write(state.join("keys/work"), "sk-secret-123").unwrap();
        std::fs::write(home.join("hello.txt"), "hello from disk\n").unwrap();
        let tools = Registry::builtin();
        let session = Session::create(
            &state,
            Opening {
                connection: "work".into(),
                model: "vendor/model".into(),
                audience: "terminal".into(),
                system: "You are Tiphys.".into(),
                tools: tools.specs(),
            },
        )
        .unwrap();
        let mut prices = PriceBook::default();
        prices.learn(
            "work",
            &[Model {
                id: "vendor/model".into(),
                context: None,
                rates: Some(Rates {
                    input: 1.0,
                    output: 2.0,
                    cache_read: None,
                    cache_write: None,
                }),
                tools: None,
            }],
        );
        Fixture {
            agent: Agent {
                provider,
                session,
                tools,
                ctx: ToolCtx {
                    state,
                    home: home.clone(),
                    cwd: home,
                },
                prices,
                local: false,
                limits: Limits {
                    rounds: 8,
                    max_tokens: None,
                },
                cancel: Arc::new(Cancel::default()),
            },
            provider: replay,
            events: Arc::default(),
            _dir: dir,
        }
    }

    fn fixture(replies: Vec<Vec<Delta>>) -> Fixture {
        let replay = Arc::new(ReplayProvider::new(replies));
        fixture_with(replay.clone(), replay)
    }

    fn says(text: &str) -> Vec<Delta> {
        vec![
            Delta::Text(text.into()),
            Delta::Usage(Usage {
                input: 1_000_000,
                output: 500_000,
                ..Usage::default()
            }),
            Delta::Done,
        ]
    }

    fn calls(calls: &[(&str, &str, &str)]) -> Vec<Delta> {
        let mut deltas: Vec<Delta> = calls
            .iter()
            .enumerate()
            .map(|(slot, (id, name, arguments))| {
                Delta::ToolCall(ToolCallPart {
                    slot: Some(slot as u32),
                    id: (*id).into(),
                    name: (*name).into(),
                    arguments: (*arguments).into(),
                })
            })
            .collect();
        deltas.push(Delta::Done);
        deltas
    }

    fn read_hello(id: &str) -> Vec<Delta> {
        calls(&[(
            id,
            "read_file",
            r#"{"path":"hello.txt","reason":"to see it"}"#,
        )])
    }

    /// The text of the tool results in the transcript, in order.
    fn results(f: &Fixture) -> Vec<String> {
        f.agent
            .session
            .messages()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_plain_answer_completes_the_turn_and_is_paid_for() {
        let mut f = fixture(vec![says("Hello.")]);
        let turn = f.turn("hi").await;

        assert_eq!(
            turn,
            Turn {
                reason: StopReason::Completed,
                text: "Hello.".into(),
                rounds: 1,
                tool_calls: 0,
                error: None
            }
        );
        assert_eq!(
            f.kinds(),
            [
                "user_message",
                "spend",
                "assistant_message",
                "turn_finished"
            ]
        );
        let messages = f.agent.session.messages();
        assert_eq!(
            messages,
            [Message::user("hi"), Message::assistant("Hello.", vec![])]
        );

        // 1M input at $1 and 0.5M output at $2.
        let (today, _) = spend::totals(&f.agent.ctx.state).unwrap();
        assert!((today.cost - 2.0).abs() < 1e-9 && today.calls == 1 && today.unpriced == 0);

        // The model was sent the session's frozen prompt and tools.
        let sent = &f.provider.requests()[0];
        assert_eq!(sent.system.as_deref(), Some("You are Tiphys."));
        assert_eq!(sent.model, "vendor/model");
        assert_eq!(sent.tools.len(), 3);
    }

    #[tokio::test]
    async fn a_tool_call_runs_and_its_result_goes_back_to_the_model() {
        let mut f = fixture(vec![read_hello("call_1"), says("It says hello.")]);
        let turn = f.turn("what is in hello.txt?").await;

        assert_eq!(
            (turn.reason, turn.rounds, turn.tool_calls),
            (StopReason::Completed, 2, 1)
        );
        assert_eq!(turn.text, "It says hello.");
        assert_eq!(
            f.kinds(),
            [
                "user_message",
                "spend",
                "tool_started",
                "tool_finished",
                "spend",
                "assistant_message",
                "turn_finished"
            ]
        );
        let started = f.events.lock().unwrap().iter().find_map(|e| match e {
            Event::ToolStarted {
                summary, reason, ..
            } => Some((summary.clone(), reason.clone())),
            _ => None,
        });
        let path = f.home().join("hello.txt");
        assert_eq!(
            started,
            Some((format!("read {}", path.display()), "to see it".into()))
        );

        // The second request carries the call and its result.
        let second = &f.provider.requests()[1];
        let last = second.messages.last().unwrap();
        assert_eq!(
            (last.role, last.content.as_str()),
            (Role::Tool, "hello from disk\n")
        );
        assert_eq!(last.tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn a_call_that_cannot_run_is_explained_to_the_model_and_the_turn_goes_on() {
        let mut f = fixture(vec![
            calls(&[
                ("a", "launch_rocket", "{}"),
                ("b", "read_file", "{\"path\":"),
                ("c", "read_file", r#"{"path":"missing.txt"}"#),
                ("d", "read_file", r#"{"path":"~/.tiphys/keys/work"}"#),
            ]),
            says("Could not do those."),
        ]);
        let turn = f.turn("go").await;
        assert_eq!((turn.reason, turn.tool_calls), (StopReason::Completed, 4));

        let results = results(&f);
        assert!(
            results[0].contains("no tool named `launch_rocket`"),
            "{}",
            results[0]
        );
        assert!(results[1].contains("not JSON"), "{}", results[1]);
        assert!(results[2].contains("missing.txt"), "{}", results[2]);
        assert!(results[3].starts_with("refused:"), "{}", results[3]);
        // The key never left the disk.
        let sent = format!("{:?}", f.provider.requests());
        assert!(!sent.contains("sk-secret-123"));
        let failures = f
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::ToolFinished { ok: false, .. }))
            .count();
        assert_eq!(failures, 4);
    }

    #[tokio::test]
    async fn the_same_call_with_the_same_result_is_noted_and_then_stopped() {
        let mut f = fixture((1..=6).map(|n| read_hello(&format!("call_{n}"))).collect());
        let turn = f.turn("loop").await;

        assert_eq!((turn.reason, turn.rounds), (StopReason::Stuck, 5));
        let results = results(&f);
        assert_eq!(results.len(), 5);
        assert_eq!(results[1], "hello from disk\n");
        assert!(results[2].contains("made this exact call several times"));
        assert!(!results[3].contains("made this exact call"));
    }

    #[tokio::test]
    async fn a_turn_stops_at_its_round_limit_and_says_so() {
        // Different files each time, so it is not a loop, just a long job.
        let replies = (0..10)
            .map(|n| {
                calls(&[(
                    &format!("c{n}"),
                    "read_file",
                    &format!(r#"{{"path":"f{n}.txt"}}"#),
                )])
            })
            .collect();
        let mut f = fixture(replies);
        let turn = f.turn("long job").await;

        assert_eq!((turn.reason, turn.rounds), (StopReason::Rounds, 8));
        assert!(f.kinds().iter().any(|kind| kind == "notice"));
        // Every call has its result, so the next turn can carry on.
        let last = f.agent.session.messages().last().unwrap();
        assert_eq!(last.role, Role::Tool);
    }

    #[tokio::test]
    async fn a_reply_cut_off_at_the_output_limit_is_asked_to_carry_on() {
        let cut = |text: &str| {
            vec![
                Delta::Text(text.into()),
                Delta::ToolCall(ToolCallPart {
                    slot: Some(0),
                    id: "x".into(),
                    name: "read_file".into(),
                    arguments: "{\"pa".into(),
                }),
                Delta::CutOff,
                Delta::Done,
            ]
        };
        let mut f = fixture(vec![cut("First half"), says("and the rest.")]);
        let turn = f.turn("write a lot").await;
        assert_eq!(
            (turn.reason, turn.rounds, turn.tool_calls),
            (StopReason::Completed, 2, 0)
        );
        let roles: Vec<Role> = f.agent.session.messages().iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
        // The half-written call was dropped, not run and not stored.
        assert!(
            f.agent
                .session
                .messages()
                .iter()
                .all(|m| m.tool_calls.is_empty())
        );

        let mut f = fixture(vec![cut("a"), cut("b"), cut("c"), says("never reached")]);
        let turn = f.turn("write a lot").await;
        assert_eq!((turn.reason, turn.rounds), (StopReason::CutOff, 3));
    }

    #[tokio::test]
    async fn a_provider_failure_ends_the_turn_with_the_error_and_keeps_the_question() {
        let replay =
            Arc::new(ReplayProvider::default().then_fail("the provider refused the key (401)"));
        let mut f = fixture_with(replay.clone(), replay);
        let turn = f.turn("hi").await;

        assert_eq!(turn.reason, StopReason::Failed);
        assert_eq!(
            turn.error.as_deref(),
            Some("the provider refused the key (401)")
        );
        assert_eq!(f.agent.session.messages(), [Message::user("hi")]);
        let last = f.events.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            last,
            Event::TurnFinished {
                reason: StopReason::Failed,
                error: Some("the provider refused the key (401)".into())
            }
        );
    }

    #[tokio::test]
    async fn a_reply_that_loses_its_end_is_a_failure_and_is_not_stored() {
        let mut f = fixture(vec![vec![Delta::Text("half an ans".into())]]);
        let turn = f.turn("hi").await;
        assert_eq!(turn.reason, StopReason::Failed);
        assert_eq!(f.agent.session.messages(), [Message::user("hi")]);
    }

    /// A provider whose reply starts and then never ends.
    struct Hanging;

    #[async_trait]
    impl Provider for Hanging {
        async fn stream(&self, _: llm::Request) -> Result<DeltaStream> {
            let start = futures_util::stream::iter(vec![Ok(Delta::Text("thinking".into()))]);
            Ok(Box::pin(start.chain(futures_util::stream::pending())))
        }
        async fn models(&self) -> Result<Vec<Model>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_cancel_stops_a_reply_that_is_still_arriving() {
        let mut f = fixture_with(Arc::new(Hanging), Arc::default());
        let cancel = f.agent.cancel.clone();
        let events = f.events.clone();
        // Cancel as soon as the first text shows up.
        let turn = f
            .agent
            .turn("hi", &move |event| {
                if matches!(event, Event::Text { .. }) {
                    cancel.cancel();
                }
                events.lock().unwrap().push(event.clone());
            })
            .await;

        assert_eq!(turn.reason, StopReason::Cancelled);
        // Nothing of the half reply is in the transcript.
        assert_eq!(f.agent.session.messages(), [Message::user("hi")]);
    }

    #[tokio::test]
    async fn a_session_carries_on_across_turns_and_restarts() {
        let mut f = fixture(vec![says("One."), says("Two.")]);
        f.turn("first").await;
        let id = f.agent.session.meta().id.clone();
        // A crash left a call without its result.
        f.agent
            .session
            .push(Message::assistant(
                "",
                vec![ToolCall {
                    id: "lost".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            ))
            .unwrap();

        f.agent.session = Session::open(&f.agent.ctx.state, &id).unwrap();
        let turn = f.turn("second").await;
        assert_eq!(turn.reason, StopReason::Completed);

        let sent = &f.provider.requests()[1];
        let said: Vec<(Role, &str)> = sent
            .messages
            .iter()
            .map(|m| (m.role, m.content.as_str()))
            .collect();
        assert_eq!(
            said,
            [
                (Role::User, "first"),
                (Role::Assistant, "One."),
                (Role::Assistant, ""),
                (Role::Tool, INTERRUPTED_RESULT),
                (Role::User, "second"),
            ]
        );
    }
}
