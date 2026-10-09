//! `tiphys -p`: one turn, no app.
//!
//! The answer goes to standard output as it streams, and what the agent does
//! along the way goes to standard error, so the answer can be piped. With
//! `--json` every event is printed as a line of JSON instead. Nobody is there
//! to approve anything, so only what runs without asking runs.
//!
//! The exit status says how the turn ended: 0 when the model finished, 1 when
//! something failed, 2 when the turn was stopped before it finished, 3 when
//! the day's spending limit stopped it.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;

use tiphys_core::client::Client;
use tiphys_core::proto::{Event, Request, StopReason};
use tiphys_core::start::{self, Resume, Start};
use tiphys_core::{Error, Result, spend};

/// What was asked for on the command line.
pub struct Run {
    pub prompt: String,
    pub connection: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub resume: bool,
    pub json: bool,
}

pub fn run(home: &Path, run: Run) -> Result<ExitCode> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Error::Io(format!("could not start the runtime: {e}")))?;
    runtime.block_on(turn(home, run))
}

/// Runs the turn in the daemon. The run is its own audience there, so it
/// never lands in the conversation the owner has open in the app.
pub fn run_attached(client: &Client, run: Run) -> Result<ExitCode> {
    if run.session.is_some() || run.connection.is_some() || run.model.is_some() {
        return Err(Error::Config(
            "--session, --connection and --model are not available while the daemon is running; \
             a one-shot run there uses the default connection, and -c continues the last one"
                .into(),
        ));
    }
    if !run.resume {
        client.send(Request::NewSession)?;
    }
    client.send(Request::Prompt { text: run.prompt })?;
    let mut printer = Printer {
        json: run.json,
        mid_line: false,
    };
    let lost = || Error::Io("the connection to the Tiphys daemon was lost".into());
    loop {
        let event = client.events.recv().map_err(|_| lost())?;
        match &event {
            // What the daemon says on attaching, and about its own state, is
            // not part of this run.
            Event::State(_) | Event::History { .. } => continue,
            // Nobody is at this terminal to say yes.
            Event::ApprovalRequested { id, .. } => client.send(Request::Approval {
                id: id.clone(),
                approve: false,
                note: Some("nobody is here to approve it".into()),
            })?,
            _ => {}
        }
        printer.show(&event);
        match event {
            Event::TurnFinished { reason, error } => {
                printer.end_line();
                return Ok(finish(run.json, reason, error.as_deref()));
            }
            // The turn never began: no connection is set up, say.
            Event::Failed { message } => return Err(Error::Config(message)),
            _ => {}
        }
    }
}

async fn turn(home: &Path, run: Run) -> Result<ExitCode> {
    let resume = match (run.session, run.resume) {
        (Some(id), _) => Resume::Id(id),
        (None, true) => Resume::Latest,
        (None, false) => Resume::New,
    };
    let mut agent = start::agent(
        home,
        Start {
            connection: run.connection,
            model: run.model,
            resume,
            audience: "oneshot".into(),
        },
    )
    .await?;

    // Ctrl-C stops the turn cleanly: the transcript is left whole.
    let cancel = agent.cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });

    let printer = Mutex::new(Printer {
        json: run.json,
        mid_line: false,
    });
    let turn = agent
        .turn(&run.prompt, &|event| printer.lock().unwrap().show(event))
        .await;
    printer.lock().unwrap().end_line();

    Ok(finish(run.json, turn.reason, turn.error.as_deref()))
}

/// Says how the turn ended, where that is not plain from the answer, and
/// turns it into an exit status.
fn finish(json: bool, reason: StopReason, error: Option<&str>) -> ExitCode {
    if !json {
        match reason {
            StopReason::Completed => {}
            StopReason::Failed => eprintln!("tiphys: {}", error.unwrap_or("the turn failed")),
            StopReason::Cancelled => eprintln!("tiphys: stopped"),
            // The notice that came with it has said why.
            StopReason::Budget => {}
            StopReason::Rounds => {
                eprintln!("tiphys: stopped at the round limit; -c -p \"continue\" carries on");
            }
            StopReason::Stuck => eprintln!("tiphys: stopped: the model kept making the same call"),
            StopReason::CutOff => {
                eprintln!("tiphys: stopped: the reply kept hitting the output limit");
            }
        }
    }
    match reason {
        StopReason::Completed => ExitCode::SUCCESS,
        StopReason::Failed => ExitCode::FAILURE,
        StopReason::Budget => ExitCode::from(3),
        _ => ExitCode::from(2),
    }
}

struct Printer {
    json: bool,
    /// Whether the answer's last character so far was not a newline.
    mid_line: bool,
}

impl Printer {
    fn show(&mut self, event: &Event) {
        if self.json {
            if let Ok(line) = serde_json::to_string(event) {
                say!("{line}");
            }
            return;
        }
        match event {
            Event::Text { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.mid_line = !text.ends_with('\n');
            }
            Event::ToolStarted {
                summary, reason, ..
            } => {
                self.end_line();
                if reason.is_empty() {
                    eprintln!("· {summary}");
                } else {
                    eprintln!("· {summary}  ({reason})");
                }
            }
            Event::ToolFinished {
                ok: false, output, ..
            } => {
                let first = output.lines().next().unwrap_or_default();
                eprintln!("  ✗ {first}");
            }
            Event::ApprovalRequested { why, .. } => {
                eprintln!("  needs approval, and nobody is here to give it: {why}");
            }
            Event::Notice { text } => {
                self.end_line();
                eprintln!("{text}");
            }
            // A call whose price is unknown is worth a line; a priced one is not.
            Event::Spend { cost: None, .. } => {
                self.end_line();
                eprintln!("  cost of this call: {}", spend::dollars(None));
            }
            _ => {}
        }
    }

    fn end_line(&mut self) {
        if self.mid_line {
            say!();
            self.mid_line = false;
        }
    }
}
