//! The `tiphys` binary.
//!
//! `--version` and `--help` are answered by the argument parser before
//! anything else runs, so they open no file and touch no network.

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tiphys_core::actionlog::{ActionLog, Entry};
use tiphys_core::client::{self, Client};
use tiphys_core::wire::{self, ONESHOT, TERMINAL};
use tiphys_core::{Error, Result, config, session, spend};
use tiphys_daemon::install;

/// Prints a line. If nothing is reading any more, as when the output was
/// piped into `head`, the program ends quietly instead of failing.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

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
    /// Run or look at the daemon.
    Daemon {
        #[command(subcommand)]
        what: DaemonCommand,
    },
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
    /// Act for the daemon as the user this is started as. The daemon starts
    /// this itself; it is not for running by hand.
    #[command(hide = true)]
    Worker,
    /// Show what today and this month have cost.
    Spend,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon here, in the foreground, until it is stopped.
    Run,
    /// Say whether a daemon is answering.
    Status,
    /// Set the daemon up as a service on this machine. Needs root.
    Install {
        /// The user who will talk to Tiphys.
        #[arg(long, value_name = "NAME")]
        owner: String,
        /// Show what would be done, and do nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop and remove the service. The users and the data are kept.
    Uninstall {
        /// Show what would be done, and do nothing.
        #[arg(long)]
        dry_run: bool,
    },
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
    // A worker has no state directory of its own; it is told the daemon's.
    if matches!(cli.command, Some(Command::Worker)) {
        return worker();
    }
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
        return match daemon(&home, ONESHOT)? {
            Some(client) => oneshot::run_attached(&client, run),
            None => oneshot::run(&home, run),
        };
    }
    match cli.command {
        Some(Command::Daemon {
            what: DaemonCommand::Run,
        }) => {
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| Error::Io(format!("could not start the runtime: {e}")))?;
            runtime.block_on(tiphys_daemon::run(&home, &config::user_home()?))?;
        }
        Some(Command::Daemon {
            what: DaemonCommand::Status,
        }) => return daemon_status(&home),
        Some(Command::Daemon {
            what: DaemonCommand::Install { owner, dry_run },
        }) => {
            let binary = std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .map_err(|e| Error::Io(format!("could not tell where this binary is: {e}")))?;
            let options = install::Options {
                owner: owner.clone(),
                binary,
            };
            let steps = install::plan(&options, &install::ThisMachine)?;
            if carry_out(&steps, dry_run)? {
                say!("Tiphys is installed and running as a service.");
                say!(
                    "{owner} was added to the {} group, which takes effect at the next login.",
                    install::DAEMON_USER
                );
                say!("Log out and in again, then run `tiphys` and add a connection.");
            }
        }
        Some(Command::Daemon {
            what: DaemonCommand::Uninstall { dry_run },
        }) => {
            if carry_out(&install::uninstall_plan(), dry_run)? {
                say!(
                    "The service is removed. The users {} and {}, and the data in {} and {}, were kept.",
                    install::DAEMON_USER,
                    install::WORK_USER,
                    install::STATE_DIR,
                    install::WORK_HOME
                );
            }
        }
        Some(Command::Doctor { live }) => return doctor(&home, live),
        Some(Command::Log { count, what }) => print_log(&home, count, what)?,
        Some(Command::Sessions) => print_sessions(&home)?,
        Some(Command::Worker) => {}
        Some(Command::Spend) => print_spend(&home)?,
        // With nothing asked for, the app: as a client of the daemon if there
        // is one, and by itself if there is not.
        None => match daemon(&home, TERMINAL)? {
            Some(client) => tiphys_tui::run_attached(&client)?,
            None => tiphys_tui::run(&home, &config::user_home()?)?,
        },
    }
    Ok(ExitCode::SUCCESS)
}

/// Connects to the daemon, if there is one to connect to. A daemon that
/// should be there and is not answering is an error; a socket file left
/// behind by one that is gone is not.
fn daemon(home: &Path, audience: &str) -> Result<Option<Client>> {
    match wire::find(home) {
        wire::Daemon::Expected(socket) => client::connect(&socket, audience).map(Some),
        wire::Daemon::Perhaps(socket) => Ok(client::connect(&socket, audience).ok()),
        wire::Daemon::None => Ok(None),
    }
}

/// Does the steps of an install or an uninstall, or with `dry_run` only shows
/// them. Returns whether they were done.
fn carry_out(steps: &[install::Step], dry_run: bool) -> Result<bool> {
    if dry_run {
        say!("Nothing is changed. This is what would be done, in order:\n");
        for (number, step) in steps.iter().enumerate() {
            say!("{}. {}\n", number + 1, install::describe(step));
        }
        return Ok(false);
    }
    if !rustix::process::geteuid().is_root() {
        return Err(Error::Config(
            "this changes users and services, so it needs root: run it with sudo, or add \
             --dry-run to see what it would do"
                .into(),
        ));
    }
    install::apply(steps)?;
    Ok(true)
}

fn daemon_status(home: &Path) -> Result<ExitCode> {
    let socket = match wire::find(home) {
        wire::Daemon::Expected(socket) | wire::Daemon::Perhaps(socket) => socket,
        wire::Daemon::None => {
            say!("No daemon is running for {}.", home.display());
            return Ok(ExitCode::FAILURE);
        }
    };
    match client::connect(&socket, TERMINAL) {
        Ok(_) => {
            say!("The daemon is answering on {}.", socket.display());
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            say!("{e}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn worker() -> Result<ExitCode> {
    let home = config::user_home()?;
    // Commands start from the worker's own home, wherever the daemon was.
    std::env::set_current_dir(&home).map_err(|e| Error::Io(format!("{}: {e}", home.display())))?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| Error::Io(format!("could not start the runtime: {e}")))?;
    runtime.block_on(tiphys_core::worker::serve(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        home,
    ))?;
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
        say!("{mark} {}: {}", check.name, check.detail);
    }
    let failed = checks.iter().filter(|check| !check.ok).count();
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        say!("{failed} of {} checks did not pass.", checks.len());
        ExitCode::FAILURE
    })
}

fn print_log(home: &Path, count: usize, what: Option<LogCommand>) -> Result<()> {
    let log = ActionLog::at(home);
    match what {
        None => {
            let entries = log.entries()?;
            if entries.is_empty() {
                say!("The action log is empty.");
            }
            let skip = entries.len().saturating_sub(count);
            for entry in &entries[skip..] {
                say!("{}", log_line(entry));
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
            say!("{json}");
        }
        Some(LogCommand::Verify) => {
            let count = log.verify()?;
            say!(
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
        say!("No sessions yet.");
    }
    for meta in sessions {
        say!(
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
    say!("Today       {today}");
    say!("This month  {month}");
    say!("Days and months are counted in UTC.");
    Ok(())
}
