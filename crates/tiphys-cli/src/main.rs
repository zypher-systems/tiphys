//! The `tiphys` binary.
//!
//! `--version` and `--help` are answered by the argument parser before
//! anything else runs, so they open no file and touch no network.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;

/// An always-on agent for your server.
#[derive(Debug, Parser)]
#[command(name = "tiphys", version, about)]
struct Cli {}

fn main() -> ExitCode {
    let Cli {} = Cli::parse();
    let home = match tiphys_core::config::home_dir() {
        Ok(home) => home,
        Err(e) => {
            eprintln!("tiphys: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The terminal app is the next thing to be built; until it is, say so
    // instead of pretending to start.
    println!(
        "Tiphys {} has no terminal app yet. Its state directory will be {}.",
        tiphys_core::VERSION,
        home.display()
    );
    ExitCode::SUCCESS
}
