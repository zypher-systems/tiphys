//! The `tiphys` binary.
//!
//! `--version` and `--help` are answered by the argument parser before
//! anything else runs, so they open no file and touch no network.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tiphys_core::{Result, config, session, spend};

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
    /// List sessions, newest first.
    Sessions,
    /// Show what today and this month have cost.
    Spend,
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
        Some(Command::Sessions) => print_sessions(&home)?,
        Some(Command::Spend) => print_spend(&home)?,
        // The terminal app is the next thing to be built; until it is, say so
        // instead of pretending to start.
        None => println!(
            "Tiphys {} has no terminal app yet. Its state directory will be {}.",
            tiphys_core::VERSION,
            home.display()
        ),
    }
    Ok(ExitCode::SUCCESS)
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
