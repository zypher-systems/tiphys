//! Running commands: the `shell` tool.
//!
//! A command is judged before it runs (see [`crate::policy::shell`]) and then
//! handed to `bash -c` in the agent's home, with no terminal and nothing on
//! standard input. It gets a time limit, and when the call ends, however it
//! ends, everything the command started is stopped with it: a turn that is
//! cancelled leaves nothing running behind it.
//!
//! The command's environment is the daemon's own, less anything that looks
//! like a secret.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{Action, Output, Tool, ToolCtx, parse_args};
use crate::config;
use crate::policy::{Verdict, shell};

/// How long a command may run unless it asks for longer.
const DEFAULT_SECONDS: u64 = 120;
/// The longest a command may ask for.
const MAX_SECONDS: u64 = 600;
/// The longest command line taken.
const MAX_COMMAND: usize = 10_000;
/// How much of each output stream is kept while the command runs.
const STREAM_KEPT: usize = 256 * 1024;
/// How much of the output goes back to the model: the start and the end,
/// since the end is where a failure says what went wrong.
const HEAD_KEPT: usize = 8_000;
const TAIL_KEPT: usize = 20_000;
/// The longest summary line.
const SUMMARY_CHARS: usize = 160;

/// Parts of a variable's name that mark it as holding a secret.
const SECRET_NAMES: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
];

pub struct Shell;

struct Run {
    command: String,
    limit: Duration,
    verdict: Verdict,
}

impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a command with bash, in your home directory. Each call is a new shell: a `cd` or a \
         variable does not carry over to the next call, so chain with `&&` in one command. There \
         is no terminal, so nothing interactive works, and whatever the command leaves running \
         is stopped when it returns. Commands that only look around run at once, and so do \
         changes inside your home. Anything else asks the owner first: sudo, package and service \
         changes, writing outside your home, scripts and programs whose effects cannot be read \
         off the command line. Prefer plain commands over clever ones; a command the owner can \
         read at a glance is approved faster. Output beyond a limit is cut in the middle."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command line."},
                "timeout": {"type": "integer", "description": "Seconds to allow, up to 600. Default 120."},
            },
            "required": ["command"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            command: String,
            timeout: Option<u64>,
        }
        let args: Args = parse_args(args)?;
        let command = args.command.trim().to_string();
        if command.is_empty() {
            return Err("`command` is empty".into());
        }
        if command.len() > MAX_COMMAND {
            return Err(format!(
                "the command is {} bytes long; put long content in a file with write_file instead",
                command.len()
            ));
        }
        Ok(Box::new(Run {
            verdict: shell::judge(&command, &ctx.cwd, ctx.places()),
            limit: Duration::from_secs(
                args.timeout
                    .unwrap_or(DEFAULT_SECONDS)
                    .clamp(1, MAX_SECONDS),
            ),
            command,
        }))
    }
}

#[async_trait]
impl Action for Run {
    fn verdict(&self) -> Verdict {
        self.verdict.clone()
    }

    fn summary(&self) -> String {
        let first = self.command.lines().next().unwrap_or_default();
        let cut: String = first.chars().take(SUMMARY_CHARS).collect();
        if cut.len() < self.command.len() {
            format!("run: {cut} …")
        } else {
            format!("run: {cut}")
        }
    }

    /// The whole command, when the summary could not hold it. The owner must
    /// be able to read all of what they are agreeing to.
    fn preview(&self) -> Option<String> {
        let whole =
            self.command.lines().count() == 1 && self.command.chars().count() <= SUMMARY_CHARS;
        (!whole).then(|| self.command.clone())
    }

    async fn run(self: Box<Self>, ctx: &ToolCtx) -> Output {
        let mut command = Command::new("bash");
        command
            .arg("-c")
            .arg(&self.command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group, so that it and everything it starts can
            // be stopped together.
            .process_group(0)
            .kill_on_drop(true);
        for name in secret_variables(ctx) {
            command.env_remove(name);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => return Output::error(format!("could not start bash: {e}")),
        };
        // From here on, leaving this function by any way stops the group:
        // finishing, timing out, or being dropped by a cancelled turn.
        let group = Group(child.id());
        let out = tokio::spawn(read_kept(child.stdout.take()));
        let err = tokio::spawn(read_kept(child.stderr.take()));

        let waited = tokio::time::timeout(self.limit, child.wait()).await;
        // A process the command left running still holds the pipes open, so
        // the group goes before the output is collected.
        drop(group);
        let (stdout, stderr) = (out.await.unwrap_or_default(), err.await.unwrap_or_default());
        let mut text = combine(&stdout, &stderr);
        match waited {
            Ok(Ok(status)) if status.success() => {
                if text.is_empty() {
                    text.push_str("(no output)");
                }
                Output::ok(text)
            }
            Ok(Ok(status)) => {
                let how = match status.code() {
                    Some(code) => format!("[exit status {code}]"),
                    None => "[stopped by a signal]".to_string(),
                };
                Output::error(join(text, &how))
            }
            Ok(Err(e)) => Output::error(join(
                text,
                &format!("[could not wait for the command: {e}]"),
            )),
            Err(_) => Output::error(join(
                text,
                &format!(
                    "[stopped after {} seconds without finishing; allow longer with `timeout`, or \
                     run something that returns]",
                    self.limit.as_secs()
                ),
            )),
        }
    }
}

/// Stops a process group when dropped.
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        let pid = self
            .0
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        if let Some(pid) = pid {
            // Already gone is the usual case, and is fine.
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
}

/// Reads a stream to its end, keeping the start and the end of it. What does
/// not fit is still read, so the command is never left blocked on a full
/// pipe.
async fn read_kept(stream: Option<impl AsyncRead + Unpin>) -> Vec<u8> {
    let Some(mut stream) = stream else {
        return Vec::new();
    };
    let mut head = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut dropped = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let mut chunk = &buf[..n];
        if head.len() < STREAM_KEPT / 2 {
            let take = chunk.len().min(STREAM_KEPT / 2 - head.len());
            head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        tail.extend_from_slice(chunk);
        if tail.len() > STREAM_KEPT {
            let excess = tail.len() - STREAM_KEPT / 2;
            tail.drain(..excess);
            dropped += excess;
        }
    }
    if dropped > 0 {
        head.extend_from_slice(format!("\n[… {dropped} bytes left out …]\n").as_bytes());
    }
    head.extend_from_slice(&tail);
    head
}

/// Standard output, then standard error under a heading, cut in the middle if
/// the two are too long together.
fn combine(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut text = stdout.trim_end().to_string();
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push_str("\n[stderr]\n");
        }
        text.push_str(stderr.trim_end());
    }
    if text.len() <= HEAD_KEPT + TAIL_KEPT {
        return text;
    }
    let mut head_end = HEAD_KEPT;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - TAIL_KEPT;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n[… {} bytes left out …]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

fn join(mut text: String, note: &str) -> String {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(note);
    text
}

/// The variables a command is not given: the ones the configuration reads
/// keys from, and any whose name says it holds a secret.
fn secret_variables(ctx: &ToolCtx) -> Vec<String> {
    let configured: Vec<String> = config::load_at(&ctx.state)
        .map(|config| {
            config
                .connections
                .into_values()
                .filter_map(|connection| connection.env_key)
                .collect()
        })
        .unwrap_or_default();
    std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| configured.contains(name) || looks_secret(name))
        .chain(configured.iter().cloned())
        .collect()
}

fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_NAMES.iter().any(|part| upper.contains(part))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::policy::Class;
    use crate::tools::{Planned, Registry};
    use std::time::Instant;

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ToolCtx,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = home.join(".tiphys");
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::write(state.join("keys/work"), "sk-secret-123").unwrap();
        Fixture {
            ctx: ToolCtx {
                state,
                home: home.clone(),
                cwd: home,
            },
            _dir: dir,
        }
    }

    /// These tests run real commands. On a machine with no bash there is
    /// nothing to run them with, and they pass without checking anything.
    fn no_bash() -> bool {
        std::process::Command::new("bash")
            .args(["-c", "true"])
            .status()
            .is_err()
    }

    fn plan(f: &Fixture, args: Value) -> Result<Planned, String> {
        let call = ToolCall {
            id: "c".into(),
            name: "shell".into(),
            arguments: args.to_string(),
        };
        Registry::builtin().plan(&call, &f.ctx)
    }

    async fn run(f: &Fixture, command: &str) -> Output {
        plan(f, json!({"command": command}))
            .unwrap()
            .action
            .run(&f.ctx)
            .await
    }

    #[tokio::test]
    async fn a_command_runs_in_home_and_its_output_comes_back() {
        if no_bash() {
            return;
        }
        let f = fixture();
        let output = run(&f, "echo hello && pwd").await;
        assert!(output.ok, "{}", output.text);
        let home = f.ctx.home.canonicalize().unwrap();
        assert_eq!(output.text, format!("hello\n{}", home.display()));

        assert_eq!(run(&f, "true").await, Output::ok("(no output)"));
        // Nothing is on standard input, so a command that reads it ends.
        assert_eq!(run(&f, "cat").await, Output::ok("(no output)"));
    }

    #[tokio::test]
    async fn a_failure_shows_its_error_output_and_its_status() {
        if no_bash() {
            return;
        }
        let f = fixture();
        let output = run(&f, "echo out; echo problem >&2; exit 3").await;
        assert!(!output.ok);
        assert_eq!(output.text, "out\n[stderr]\nproblem\n[exit status 3]");

        let quiet = run(&f, "exit 1").await;
        assert_eq!((quiet.ok, quiet.text.as_str()), (false, "[exit status 1]"));
    }

    #[tokio::test]
    async fn a_command_that_runs_too_long_is_stopped_with_everything_it_started() {
        if no_bash() {
            return;
        }
        let f = fixture();
        let marker = f.ctx.home.join("still-running");
        // A child that would write a file after the parent has been stopped.
        let command = format!(
            "(sleep 2; touch {}) & echo started; sleep 30",
            marker.display()
        );
        // The judge would ask about a subshell; this test is about running.
        let action = Run {
            command,
            limit: Duration::from_secs(1),
            verdict: Verdict::observe(),
        };
        let started = Instant::now();
        let output = Box::new(action).run(&f.ctx).await;

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!output.ok);
        assert!(
            output.text.starts_with("started\n[stopped after 1 seconds"),
            "{}",
            output.text
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process outlived the command");
    }

    #[tokio::test]
    async fn dropping_the_call_stops_the_command() {
        if no_bash() {
            return;
        }
        let f = fixture();
        let marker = f.ctx.home.join("finished");
        let action = Run {
            command: format!("sleep 2; touch {}", marker.display()),
            limit: Duration::from_secs(30),
            verdict: Verdict::observe(),
        };
        // What a cancelled turn does: stop waiting for the action.
        let cancelled =
            tokio::time::timeout(Duration::from_millis(300), Box::new(action).run(&f.ctx)).await;
        assert!(cancelled.is_err());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the command went on after its call was dropped"
        );
    }

    #[tokio::test]
    async fn long_output_keeps_its_start_and_its_end() {
        if no_bash() {
            return;
        }
        let f = fixture();
        let output = run(&f, "seq 1 20000; echo the-end").await;
        assert!(output.ok);
        assert!(
            output.text.starts_with("1\n2\n3\n"),
            "{}",
            &output.text[..40]
        );
        assert!(
            output.text.ends_with("20000\nthe-end"),
            "{}",
            &output.text[output.text.len() - 40..]
        );
        assert!(output.text.contains("bytes left out"));
        assert!(output.text.len() < HEAD_KEPT + TAIL_KEPT + 100);
    }

    #[test]
    fn a_command_is_judged_when_it_is_planned() {
        let f = fixture();
        let cases = [
            ("ls -la", Class::Observe),
            ("echo hi > notes.txt", Class::Change),
            ("sudo apt update", Class::System),
            ("python3 x.py", Class::System),
            ("cat ~/.tiphys/keys/work", Class::Never),
            ("rm -rf ~", Class::Never),
        ];
        for (command, expected) in cases {
            let planned = plan(&f, json!({"command": command})).unwrap();
            assert_eq!(planned.action.verdict().class, expected, "{command}");
            assert_eq!(planned.action.summary(), format!("run: {command}"));
            assert_eq!(planned.action.preview(), None);
        }
    }

    #[test]
    fn a_long_or_many_line_command_is_shown_whole_in_the_preview() {
        let f = fixture();
        let script = "echo one\necho two";
        let planned = plan(&f, json!({"command": script})).unwrap();
        assert_eq!(planned.action.summary(), "run: echo one …");
        assert_eq!(planned.action.preview().as_deref(), Some(script));

        let long = format!("echo {}", "x".repeat(300));
        let planned = plan(&f, json!({"command": long})).unwrap();
        assert!(planned.action.summary().ends_with(" …"));
        assert_eq!(planned.action.preview(), Some(long));
    }

    #[test]
    fn a_command_that_cannot_be_planned_says_why() {
        let f = fixture();
        let huge = "x".repeat(MAX_COMMAND + 1);
        let cases = [
            (json!({"command": "  "}), "`command` is empty"),
            (json!({"command": huge}), "put long content in a file"),
            (json!({}), "missing field `command`"),
            (json!({"command": "ls", "cwd": "/"}), "unknown field `cwd`"),
        ];
        for (args, expected) in cases {
            let Err(message) = plan(&f, args) else {
                panic!("planned");
            };
            assert!(message.contains(expected), "{message}");
        }
        // The time limit is kept within bounds.
        let planned = plan(&f, json!({"command": "ls", "timeout": 100_000})).unwrap();
        assert_eq!(planned.action.verdict().class, Class::Observe);
    }

    #[test]
    fn variables_that_hold_secrets_are_recognised_by_name() {
        for name in [
            "OPENROUTER_API_KEY",
            "GITHUB_TOKEN",
            "aws_secret_access_key",
            "DB_PASSWORD",
            "MY_APIKEY",
        ] {
            assert!(looks_secret(name), "{name}");
        }
        for name in [
            "PATH",
            "HOME",
            "LANG",
            "TIPHYS_HOME",
            "EDITOR",
            "KEYBOARD_LAYOUT",
        ] {
            assert!(!looks_secret(name), "{name}");
        }
    }

    #[test]
    fn the_two_streams_are_joined_and_cut_in_the_middle() {
        assert_eq!(combine(b"out\n", b""), "out");
        assert_eq!(combine(b"", b"err\n"), "err");
        assert_eq!(combine(b"out\n", b"err\n"), "out\n[stderr]\nerr");
        assert_eq!(combine(b"", b""), "");
        let long = "é".repeat(HEAD_KEPT + TAIL_KEPT);
        let cut = combine(long.as_bytes(), b"");
        assert!(cut.contains("bytes left out") && cut.len() < long.len());
    }
}
