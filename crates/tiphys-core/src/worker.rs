//! Acting as another user.
//!
//! An installed Tiphys is two users. The daemon runs as one and keeps the
//! keys, the sessions and the action log. Everything the agent does to the
//! machine is done by a worker: this same program, started as a second user
//! who cannot read any of that. A command the agent runs is that second user
//! too, so the key store is out of its reach by the operating system's own
//! rules, whatever the command is.
//!
//! The daemon starts the worker with a command from its configuration,
//! `sudo -n -H -u tiphys /usr/local/bin/tiphys worker` on an installed
//! system, and talks to it over its standard input and output, one JSON
//! object per line:
//!
//! ```text
//! → {"op":"init","state":"/var/lib/tiphysd"}
//! ← {"op":"ready","machine":{"host":"argo","system":"Ubuntu 24.04","user":"tiphys","home":"/home/tiphys"}}
//! → {"op":"plan","id":1,"call":{"id":"c1","name":"shell","arguments":"{\"command\":\"df -h\"}"}}
//! ← {"op":"planned","id":1,"verdict":{"class":"observe","why":""},"summary":"run: df -h","reason":""}
//! → {"op":"run","id":1}
//! ← {"op":"ran","id":1,"output":{"text":"…","ok":true}}
//! ```
//!
//! The worker plans as well as runs, because planning reads the files it is
//! about. When its input closes, the worker stops whatever it is running and
//! ends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::llm::ToolCall;
use crate::policy::Verdict;
use crate::prompt::Machine;
use crate::runner::{Planned, Runner};
use crate::tools::{Action, Output, Registry, ToolCtx};
use crate::{Error, Result};

/// How long a worker has to say it is ready.
const READY_WITHIN: Duration = Duration::from_secs(20);

/// What the daemon says to the worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ToWorker {
    /// The first message: where the daemon's state is, so that the worker's
    /// rules can refuse to go near it.
    Init {
        state: PathBuf,
    },
    Plan {
        id: u64,
        call: ToolCall,
    },
    Run {
        id: u64,
    },
    /// A planned call will not be run.
    Discard {
        id: u64,
    },
    /// Stop a call that is running.
    Cancel {
        id: u64,
    },
}

/// What the worker says back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FromWorker {
    Ready {
        machine: Machine,
    },
    Planned {
        id: u64,
        verdict: Verdict,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        reason: String,
    },
    /// The call could not be planned; the message is for the model.
    Unplanned {
        id: u64,
        message: String,
    },
    Ran {
        id: u64,
        output: Output,
    },
}

fn line<T: Serialize>(message: &T) -> String {
    let mut line = serde_json::to_string(message).unwrap_or_default();
    line.push('\n');
    line
}

/// Runs the worker's side: reads what the daemon asks on `input` and answers
/// on `output`, acting as the user this process runs as, whose home is
/// `home`. Returns when the input closes, after stopping anything running.
pub async fn serve(
    input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin + Send + 'static,
    home: PathBuf,
) -> Result<()> {
    let mut lines = input.lines();
    let first = lines
        .next_line()
        .await
        .map_err(|e| Error::Io(format!("the worker's input: {e}")))?;
    let state = match first.as_deref().map(serde_json::from_str::<ToWorker>) {
        Some(Ok(ToWorker::Init { state })) => state,
        _ => {
            return Err(Error::Config(
                "a worker is started by the Tiphys daemon, not by hand".into(),
            ));
        }
    };
    let ctx = Arc::new(ToolCtx {
        state,
        cwd: home.clone(),
        home,
    });
    let registry = Registry::builtin();

    // One task writes, so that answers from calls running side by side never
    // interleave within a line.
    let (answers, mut outgoing) = unbounded_channel::<FromWorker>();
    let writer = tokio::spawn(async move {
        while let Some(answer) = outgoing.recv().await {
            if output.write_all(line(&answer).as_bytes()).await.is_err()
                || output.flush().await.is_err()
            {
                return;
            }
        }
        // Closing the output is how the daemon learns the worker has ended.
        let _ = output.shutdown().await;
    });
    let _ = answers.send(FromWorker::Ready {
        machine: Machine::detect(&ctx.home),
    });

    let mut planned: HashMap<u64, Box<dyn Action>> = HashMap::new();
    let mut running = JoinSet::new();
    let mut handles = HashMap::new();
    loop {
        let message = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => serde_json::from_str::<ToWorker>(&line),
                // The daemon is gone, or stopped listening.
                _ => break,
            },
            // Finished calls are reaped as they end.
            Some(finished) = running.join_next(), if !running.is_empty() => {
                if let Ok(id) = finished {
                    handles.remove(&id);
                }
                continue;
            }
        };
        match message {
            Ok(ToWorker::Plan { id, call }) => {
                let answer = match registry.plan(&call, &ctx) {
                    Ok(plan) => {
                        let answer = FromWorker::Planned {
                            id,
                            verdict: plan.action.verdict(),
                            summary: plan.action.summary(),
                            preview: plan.action.preview(),
                            reason: plan.reason,
                        };
                        planned.insert(id, plan.action);
                        answer
                    }
                    Err(message) => FromWorker::Unplanned { id, message },
                };
                let _ = answers.send(answer);
            }
            Ok(ToWorker::Run { id }) => {
                let Some(action) = planned.remove(&id) else {
                    let output = Output::error("there is no such planned action");
                    let _ = answers.send(FromWorker::Ran { id, output });
                    continue;
                };
                let (ctx, answers) = (ctx.clone(), answers.clone());
                let handle = running.spawn(async move {
                    let output = action.run(&ctx).await;
                    let _ = answers.send(FromWorker::Ran { id, output });
                    id
                });
                handles.insert(id, handle);
            }
            Ok(ToWorker::Discard { id }) => {
                planned.remove(&id);
            }
            // Aborting the task drops the action, which stops what it started.
            Ok(ToWorker::Cancel { id }) => {
                if let Some(handle) = handles.remove(&id) {
                    handle.abort();
                }
            }
            // A second init, or something this version does not know.
            Ok(ToWorker::Init { .. }) | Err(_) => {}
        }
    }
    // Stop what is running and wait for it to be stopped, so that no command
    // is left behind when the worker ends.
    running.shutdown().await;
    drop(answers);
    let _ = writer.await;
    Ok(())
}

/// The questions waiting for the worker's answer, by id. `None` once the
/// worker has gone: a question asked after that would wait for ever, so it is
/// not taken.
type Waiting = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<FromWorker>>>>>;

/// Plans and runs in a worker process.
pub struct WorkerRunner {
    machine: Machine,
    requests: UnboundedSender<ToWorker>,
    waiting: Waiting,
    next: AtomicU64,
    /// The worker's process, when this runner started one. Held so that it
    /// is reaped; the worker itself ends when its input closes.
    _child: Option<tokio::process::Child>,
}

impl WorkerRunner {
    /// Starts a worker with `command` and waits for it to be ready.
    pub async fn spawn(command: &[String], state: &Path) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .ok_or_else(|| Error::Config("the worker's command is empty".into()))?;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // What the worker, or sudo before it, has to say goes where the
            // daemon's own errors go.
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                Error::Config(format!("could not start the worker with `{program}`: {e}"))
            })?;
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Error::Io("the worker has no input or output".into()));
        };
        let mut runner = Self::over(BufReader::new(output), input, state)
            .await
            .map_err(|e| {
                Error::Config(format!(
                    "the worker did not start ({e}); it is started with `{}`, which has to work \
                 without a password",
                    command.join(" ")
                ))
            })?;
        runner._child = Some(child);
        Ok(runner)
    }

    /// Talks to a worker over the two ends given, and waits for it to be
    /// ready.
    pub async fn over(
        mut from_worker: impl AsyncBufRead + Unpin + Send + 'static,
        mut to_worker: impl AsyncWrite + Unpin + Send + 'static,
        state: &Path,
    ) -> Result<Self> {
        let io = |e: std::io::Error| Error::Io(e.to_string());
        let init = ToWorker::Init {
            state: state.to_path_buf(),
        };
        to_worker
            .write_all(line(&init).as_bytes())
            .await
            .map_err(io)?;
        to_worker.flush().await.map_err(io)?;
        let mut first = String::new();
        let read = tokio::time::timeout(READY_WITHIN, from_worker.read_line(&mut first)).await;
        let machine = match read {
            Ok(Ok(_)) => match serde_json::from_str::<FromWorker>(&first) {
                Ok(FromWorker::Ready { machine }) => machine,
                _ => return Err(Error::Io("it did not say it was ready".into())),
            },
            Ok(Err(e)) => return Err(io(e)),
            Err(_) => return Err(Error::Io("it did not answer in time".into())),
        };

        let (requests, mut outgoing) = unbounded_channel::<ToWorker>();
        tokio::spawn(async move {
            while let Some(request) = outgoing.recv().await {
                if to_worker
                    .write_all(line(&request).as_bytes())
                    .await
                    .is_err()
                    || to_worker.flush().await.is_err()
                {
                    return;
                }
            }
            // Closing the worker's input is what ends it.
            let _ = to_worker.shutdown().await;
        });
        let waiting: Waiting = Arc::new(Mutex::new(Some(HashMap::new())));
        let answered = waiting.clone();
        tokio::spawn(async move {
            let mut lines = from_worker.lines();
            while let Ok(Some(text)) = lines.next_line().await {
                let Ok(answer) = serde_json::from_str::<FromWorker>(&text) else {
                    continue;
                };
                let id = match &answer {
                    FromWorker::Planned { id, .. }
                    | FromWorker::Unplanned { id, .. }
                    | FromWorker::Ran { id, .. } => *id,
                    FromWorker::Ready { .. } => continue,
                };
                let asker = answered
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|waiting| waiting.remove(&id));
                if let Some(asker) = asker {
                    let _ = asker.send(answer);
                }
            }
            // The worker is gone. Whoever is waiting is told so by their
            // question being dropped, and no more are taken.
            *answered.lock().unwrap() = None;
        });
        Ok(Self {
            machine,
            requests,
            waiting,
            next: AtomicU64::new(1),
            _child: None,
        })
    }

    fn forget(&self, id: u64) {
        if let Some(waiting) = self.waiting.lock().unwrap().as_mut() {
            waiting.remove(&id);
        }
    }

    /// Sends a request and waits for the answer with its id.
    async fn ask(&self, id: u64, request: ToWorker) -> Option<FromWorker> {
        let (asker, answer) = oneshot::channel();
        self.waiting.lock().unwrap().as_mut()?.insert(id, asker);
        if self.requests.send(request).is_err() {
            self.forget(id);
            return None;
        }
        answer.await.ok()
    }
}

const GONE: &str = "the worker that runs Tiphys's actions has stopped";

#[async_trait]
impl Runner for WorkerRunner {
    fn machine(&self) -> Machine {
        self.machine.clone()
    }

    async fn plan(&self, call: &ToolCall) -> std::result::Result<Planned, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let request = ToWorker::Plan {
            id,
            call: call.clone(),
        };
        match self.ask(id, request).await {
            Some(FromWorker::Planned {
                verdict,
                summary,
                preview,
                reason,
                ..
            }) => Ok(Planned {
                ticket: id,
                verdict,
                summary,
                preview,
                reason,
            }),
            Some(FromWorker::Unplanned { message, .. }) => Err(message),
            _ => Err(GONE.into()),
        }
    }

    async fn run(&self, ticket: u64) -> Output {
        // If this future is dropped before the worker answers, the turn was
        // cancelled, and the worker is told to stop the call.
        struct StopIfDropped<'a> {
            runner: &'a WorkerRunner,
            id: u64,
            done: bool,
        }
        impl Drop for StopIfDropped<'_> {
            fn drop(&mut self) {
                if !self.done {
                    self.runner.forget(self.id);
                    let _ = self.runner.requests.send(ToWorker::Cancel { id: self.id });
                }
            }
        }
        let mut guard = StopIfDropped {
            runner: self,
            id: ticket,
            done: false,
        };
        let answer = self.ask(ticket, ToWorker::Run { id: ticket }).await;
        guard.done = true;
        match answer {
            Some(FromWorker::Ran { output, .. }) => output,
            _ => Output::error(GONE),
        }
    }

    fn discard(&self, ticket: u64) {
        let _ = self.requests.send(ToWorker::Discard { id: ticket });
    }

    fn is_alive(&self) -> bool {
        self.waiting.lock().unwrap().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Class;
    use std::time::Instant;

    struct Fixture {
        dir: tempfile::TempDir,
        runner: WorkerRunner,
        worker: tokio::task::JoinHandle<Result<()>>,
    }

    impl Fixture {
        fn home(&self) -> PathBuf {
            self.dir.path().join("work")
        }
    }

    /// A worker served in this process, on the other end of two pipes: the
    /// same code on both sides as when the worker is another process.
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("work");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::write(home.join("hello.txt"), "hello\n").unwrap();
        let (daemon_end, worker_end) = tokio::io::duplex(64 * 1024);
        let (worker_in, worker_out) = tokio::io::split(worker_end);
        let worker = tokio::spawn(serve(BufReader::new(worker_in), worker_out, home));
        let (from_worker, to_worker) = tokio::io::split(daemon_end);
        let runner = WorkerRunner::over(BufReader::new(from_worker), to_worker, &state)
            .await
            .unwrap();
        Fixture {
            dir,
            runner,
            worker,
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: arguments.to_string(),
        }
    }

    fn no_bash() -> bool {
        std::process::Command::new("bash")
            .args(["-c", "true"])
            .status()
            .is_err()
    }

    #[tokio::test]
    async fn a_call_is_planned_and_run_on_the_workers_side() {
        let f = fixture().await;
        assert_eq!(f.runner.machine().home, f.home());

        let planned = f
            .runner
            .plan(&call(
                "read_file",
                serde_json::json!({"path": "hello.txt", "reason": "to see it"}),
            ))
            .await
            .unwrap();
        assert_eq!(planned.verdict, Verdict::observe());
        assert_eq!(planned.reason, "to see it");
        assert!(
            planned.summary.ends_with("work/hello.txt"),
            "{}",
            planned.summary
        );
        assert_eq!(f.runner.run(planned.ticket).await, Output::ok("hello\n"));

        let written = f
            .runner
            .plan(&call(
                "write_file",
                serde_json::json!({"path": "new.txt", "content": "made by the worker\n"}),
            ))
            .await
            .unwrap();
        assert!(
            written
                .preview
                .as_ref()
                .unwrap()
                .contains("+made by the worker")
        );
        assert!(f.runner.run(written.ticket).await.ok);
        assert_eq!(
            std::fs::read_to_string(f.home().join("new.txt")).unwrap(),
            "made by the worker\n"
        );
    }

    #[tokio::test]
    async fn the_workers_rules_keep_it_away_from_the_daemons_state() {
        let f = fixture().await;
        let state = f.dir.path().join("state");
        let reads_key = call(
            "read_file",
            serde_json::json!({"path": state.join("keys/work")}),
        );
        assert_eq!(
            f.runner.plan(&reads_key).await.unwrap().verdict.class,
            Class::Never
        );
        let writes_state = call(
            "write_file",
            serde_json::json!({"path": state.join("config.toml"), "content": "x"}),
        );
        assert_eq!(
            f.runner.plan(&writes_state).await.unwrap().verdict.class,
            Class::Never
        );
        let command = format!("cat {}", state.join("keys/work").display());
        assert_eq!(
            f.runner
                .plan(&call("shell", serde_json::json!({"command": command})))
                .await
                .unwrap()
                .verdict
                .class,
            Class::Never
        );
    }

    #[tokio::test]
    async fn what_cannot_be_planned_or_was_discarded_is_said() {
        let f = fixture().await;
        let message = f
            .runner
            .plan(&call("launch_rocket", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(
            message.contains("no tool named `launch_rocket`"),
            "{message}"
        );

        let planned = f
            .runner
            .plan(&call("read_file", serde_json::json!({"path": "hello.txt"})))
            .await
            .unwrap();
        f.runner.discard(planned.ticket);
        let output = f.runner.run(planned.ticket).await;
        assert!(
            !output.ok && output.text.contains("no such planned action"),
            "{}",
            output.text
        );
    }

    #[tokio::test]
    async fn calls_run_side_by_side_and_each_gets_its_own_answer() {
        if no_bash() {
            return;
        }
        let f = fixture().await;
        let slow = f
            .runner
            .plan(&call(
                "shell",
                serde_json::json!({"command": "sleep 1; echo slow"}),
            ))
            .await
            .unwrap();
        let fast = f
            .runner
            .plan(&call("shell", serde_json::json!({"command": "echo fast"})))
            .await
            .unwrap();
        let started = Instant::now();
        let (slow, fast) = tokio::join!(f.runner.run(slow.ticket), f.runner.run(fast.ticket));
        assert_eq!((slow.text.as_str(), fast.text.as_str()), ("slow", "fast"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dropping_a_run_stops_the_command_on_the_workers_side() {
        if no_bash() {
            return;
        }
        let f = fixture().await;
        let marker = f.home().join("finished");
        let command = format!("sleep 2; touch {}", marker.display());
        let planned = f
            .runner
            .plan(&call("shell", serde_json::json!({"command": command})))
            .await
            .unwrap();
        // What a cancelled turn does: stop waiting.
        let cancelled =
            tokio::time::timeout(Duration::from_millis(300), f.runner.run(planned.ticket)).await;
        assert!(cancelled.is_err());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the command went on after its run was dropped"
        );
        // The worker is still there for the next call.
        let next = f
            .runner
            .plan(&call(
                "shell",
                serde_json::json!({"command": "echo still here"}),
            ))
            .await
            .unwrap();
        assert_eq!(f.runner.run(next.ticket).await.text, "still here");
    }

    #[tokio::test]
    async fn a_worker_whose_daemon_goes_away_stops_what_it_was_running_and_ends() {
        if no_bash() {
            return;
        }
        let Fixture {
            dir,
            runner,
            worker,
        } = fixture().await;
        let marker = dir.path().join("work/finished");
        let command = format!("sleep 2; touch {}", marker.display());
        let planned = runner
            .plan(&call("shell", serde_json::json!({"command": command})))
            .await
            .unwrap();
        let running = tokio::spawn(async move {
            runner.run(planned.ticket).await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // The daemon dies: its end of the pipes closes.
        running.abort();
        let _ = running.await;
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a command outlived the worker");
    }

    #[tokio::test]
    async fn a_daemon_whose_worker_goes_away_is_told_plainly() {
        let Fixture { runner, worker, .. } = fixture().await;
        assert!(runner.is_alive());
        worker.abort();
        let _ = worker.await;
        // The reader notices on its own task; give it the moment it needs.
        for _ in 0..200 {
            if !runner.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!runner.is_alive());
        let message = runner
            .plan(&call("read_file", serde_json::json!({"path": "hello.txt"})))
            .await
            .unwrap_err();
        assert_eq!(message, GONE);
        assert_eq!(runner.run(99).await, Output::error(GONE));
    }

    #[tokio::test]
    async fn a_worker_started_by_hand_or_one_that_never_answers_is_an_error() {
        let by_hand = serve(
            BufReader::new(&b"hello\n"[..]),
            tokio::io::sink(),
            "/tmp".into(),
        )
        .await;
        assert!(by_hand.unwrap_err().to_string().contains("not by hand"));

        // Something that is not a worker on the other end.
        let state = Path::new("/var/lib/tiphysd");
        let silent = WorkerRunner::over(
            BufReader::new(&b"sudo: a password is required\n"[..]),
            tokio::io::sink(),
            state,
        )
        .await;
        assert!(silent.is_err());
        let missing =
            WorkerRunner::spawn(&["/nonexistent/tiphys".into(), "worker".into()], state).await;
        let Err(e) = missing else {
            panic!("started a worker that does not exist");
        };
        assert!(e.to_string().contains("could not start the worker"), "{e}");
        assert!(WorkerRunner::spawn(&[], state).await.is_err());
    }
}
