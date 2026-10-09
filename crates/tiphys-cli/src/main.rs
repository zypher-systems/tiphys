//! The `tiphys` binary.
//!
//! `--version` and `--help` are answered by the argument parser before
//! anything else runs, so they open no file and touch no network.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tiphys_core::actionlog::{ActionLog, Entry};
use tiphys_core::{Error, Result, config, session, spend};

mod oneshot;

/// An always-on agent for your server.
#[derive(Debug, Parser)]
#[command(name = "tiphys", version, about)]
struct Cli {
    /// Run one turn with this message and print the answer, with no app.
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    prompt: Option<String>,

    /// With -p: carry on the most recent session instead of starting one.
    #[arg(
        short = 'c',
        long = "continue",
        requires = "prompt",
        conflicts_with = "session"
    )]
    resume: bool,

    /// With -p: carry on this session.
    #[arg(long, value_name = "ID", requires = "prompt")]
    session: Option<String>,

    /// With -p: the connection to use for a new session.
    #[arg(long, value_name = "NAME", requires = "prompt")]
    connection: Option<String>,

    /// With -p: the model to use for a new session.
    #[arg(long, value_name = "ID", requires = "prompt")]
    model: Option<String>,

    /// With -p: print every event as a line of JSON instead of the answer.
    #[arg(long, requires = "prompt")]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that this installation is in working order.
    Doctor {
        /// Also make a real, paid tool call on the default connection.
        #[arg(long)]
        live: bool,
    },
    /// Show the action log: every tool call, and how it came to run or not.
    Log {
        /// How many of the most recent entries to list.
        #[arg(short = 'n', default_value_t = 20)]
        count: usize,
        #[command(subcommand)]
        what: Option<LogCommand>,
    },
    /// List sessions, newest first.
    Sessions,
    /// Show what today and this month have cost.
    Spend,
}

#[derive(Debug, Subcommand)]
enum LogCommand {
    /// One entry in full.
    Show { seq: u64 },
    /// Check that no entry has been changed or removed.
    Verify,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tiphys: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let home = config::home_dir()?;
    if let Some(prompt) = cli.prompt {
        let run = oneshot::Run {
            prompt,
            connection: cli.connection,
            model: cli.model,
            session: cli.session,
            resume: cli.resume,
            json: cli.json,
        };
        return oneshot::run(&home, run);
    }
    match cli.command {
        Some(Command::Doctor { live }) => return doctor(&home, live),
        Some(Command::Log { count, what }) => print_log(&home, count, what)?,
        Some(Command::Sessions) => print_sessions(&home)?,
        Some(Command::Spend) => print_spend(&home)?,
        // With nothing asked for, the app.
        None => tiphys_tui::run(&home, &config::user_home()?)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn doctor(home: &Path, live: bool) -> Result<ExitCode> {
    let mut checks = tiphys_core::doctor::run(home);
    if live {
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Io(format!("could not start the runtime: {e}")))?;
        checks.push(runtime.block_on(tiphys_core::doctor::live(
            home,
            &tiphys_core::llm::ChatConnect,
        )));
    }
    for check in &checks {
        let mark = if check.ok { "✓" } else { "✗" };
        println!("{mark} {}: {}", check.name, check.detail);
    }
    let failed = checks.iter().filter(|check| !check.ok).count();
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        println!("{failed} of {} checks did not pass.", checks.len());
        ExitCode::FAILURE
    })
}

fn print_log(home: &Path, count: usize, what: Option<LogCommand>) -> Result<()> {
    let log = ActionLog::at(home);
    match what {
        None => {
            let entries = log.entries()?;
            if entries.is_empty() {
                println!("The action log is empty.");
            }
            let skip = entries.len().saturating_sub(count);
            for entry in &entries[skip..] {
                println!("{}", log_line(entry));
            }
        }
        Some(LogCommand::Show { seq }) => {
            let entries = log.entries()?;
            let entry = entries
                .iter()
                .find(|entry| entry.seq == seq)
                .ok_or_else(|| Error::Config(format!("the action log has no entry {seq}")))?;
            let json = serde_json::to_string_pretty(entry)
                .map_err(|e| Error::Io(format!("cannot show the entry: {e}")))?;
            println!("{json}");
        }
        Some(LogCommand::Verify) => {
            let count = log.verify()?;
            println!(
                "{count} entries, each following from the one before. Nothing has been changed or removed."
            );
        }
    }
    Ok(())
}

/// One entry on one line: its number, when, how it was let through, whether
/// it worked, and what it was.
fn log_line(entry: &Entry) -> String {
    let lower = |value: &dyn std::fmt::Debug| format!("{value:?}").to_lowercase();
    format!(
        "{:>5}  {}  {:<8}  {:<7}  {}  {}",
        entry.seq,
        entry.at.format("%Y-%m-%d %H:%M:%S"),
        lower(&entry.gate),
        entry
            .class
            .map_or_else(|| "-".to_string(), |class| lower(&class)),
        if entry.ok { "ok    " } else { "failed" },
        entry.summary,
    )
}

fn print_sessions(home: &Path) -> Result<()> {
    let sessions = session::list(home)?;
    if sessions.is_empty() {
        println!("No sessions yet.");
    }
    for meta in sessions {
        println!(
            "{}  {}  {}  {}",
            meta.id,
            meta.created.format("%Y-%m-%d %H:%M"),
            meta.model,
            meta.title
        );
    }
    Ok(())
}

fn print_spend(home: &Path) -> Result<()> {
    let (today, month) = spend::totals(home)?;
    println!("Today       {today}");
    println!("This month  {month}");
    println!("Days and months are counted in UTC.");
    Ok(())
}
