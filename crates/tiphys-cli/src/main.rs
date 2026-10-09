//! The `tiphys` binary.
//!
//! `--version` and `--help` are answered by the argument parser before
//! anything else runs, so they open no file and touch no network.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tiphys_core::{Result, config, spend};

/// An always-on agent for your server.
#[derive(Debug, Parser)]
#[command(name = "tiphys", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show what today and this month have cost.
    Spend,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tiphys: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = config::home_dir()?;
    match cli.command {
        Some(Command::Spend) => print_spend(&home),
        // The terminal app is the next thing to be built; until it is, say so
        // instead of pretending to start.
        None => {
            println!(
                "Tiphys {} has no terminal app yet. Its state directory will be {}.",
                tiphys_core::VERSION,
                home.display()
            );
            Ok(())
        }
    }
}

fn print_spend(home: &Path) -> Result<()> {
    let (today, month) = spend::totals(home)?;
    println!("Today       {today}");
    println!("This month  {month}");
    println!("Days and months are counted in UTC.");
    Ok(())
}
