//! What Tiphys says about itself: the action log, spending, sessions, and
//! whether the installation is in order.
//!
//! These are read from the state directory. An owner whose Tiphys is
//! installed as a service cannot read that directory, so the same reports can
//! be asked of the daemon, which renders them here and sends the text.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::actionlog::{ActionLog, Entry};
use crate::doctor::{self, Check};
use crate::llm::Connect;
use crate::runner::Runner;
use crate::worker::WorkerRunner;
use crate::{Error, Result, config, session, spend};

/// A report to ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "report", rename_all = "snake_case")]
pub enum Report {
    /// The most recent entries of the action log.
    Log {
        count: usize,
    },
    /// One entry of the action log, in full.
    LogEntry {
        seq: u64,
    },
    /// Whether the action log's chain holds.
    LogVerify,
    Sessions,
    Spend,
    /// Whether the installation is in order. With `live`, a real, paid tool
    /// call is made on the default connection as well.
    Doctor {
        live: bool,
    },
}

/// A report as text, and whether what it found is good news. A failed check
/// is not an error: the report was made, and it says what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub ok: bool,
}

fn good(text: String) -> Rendered {
    Rendered { text, ok: true }
}

/// Makes a report from the state directory `home`.
pub async fn render(home: &Path, report: &Report, connect: &dyn Connect) -> Result<Rendered> {
    match report {
        Report::Log { count } => {
            let entries = ActionLog::at(home).entries()?;
            if entries.is_empty() {
                return Ok(good("The action log is empty.".into()));
            }
            let skip = entries.len().saturating_sub(*count);
            let lines: Vec<String> = entries[skip..].iter().map(log_line).collect();
            Ok(good(lines.join("\n")))
        }
        Report::LogEntry { seq } => {
            let entries = ActionLog::at(home).entries()?;
            let entry = entries
                .iter()
                .find(|entry| entry.seq == *seq)
                .ok_or_else(|| Error::Config(format!("the action log has no entry {seq}")))?;
            serde_json::to_string_pretty(entry)
                .map(good)
                .map_err(|e| Error::Io(format!("cannot show the entry: {e}")))
        }
        Report::LogVerify => {
            let count = ActionLog::at(home).verify()?;
            Ok(good(format!(
                "{count} entries, each following from the one before. Nothing has been changed or removed."
            )))
        }
        Report::Sessions => {
            let sessions = session::list(home)?;
            if sessions.is_empty() {
                return Ok(good("No sessions yet.".into()));
            }
            let lines: Vec<String> = sessions
                .iter()
                .map(|meta| {
                    format!(
                        "{}  {}  {:<9}  {}  {}",
                        meta.id,
                        meta.created.format("%Y-%m-%d %H:%M"),
                        meta.audience,
                        meta.model,
                        meta.title
                    )
                })
                .collect();
            Ok(good(lines.join("\n")))
        }
        Report::Spend => {
            let (today, month) = spend::totals(home)?;
            Ok(good(format!(
                "Today       {today}\nThis month  {month}\nDays and months are counted in UTC."
            )))
        }
        Report::Doctor { live } => {
            let mut checks = doctor::run(home);
            checks.extend(worker(home).await);
            if *live {
                checks.push(doctor::live(home, connect).await);
            }
            Ok(checks_text(&checks))
        }
    }
}

/// Checks the worker, where one is configured, by starting it: that proves
/// the command works and says who the agent acts as.
async fn worker(home: &Path) -> Option<Check> {
    const NAME: &str = "worker";
    let command = config::load_at(home).ok()?.daemon.worker;
    if command.is_empty() {
        return None;
    }
    Some(match WorkerRunner::spawn(&command, home).await {
        Ok(worker) => {
            let machine = worker.machine();
            Check {
                name: NAME.into(),
                ok: true,
                detail: format!(
                    "the agent acts as `{}`, whose home is {}",
                    machine.user,
                    machine.home.display()
                ),
            }
        }
        Err(e) => Check {
            name: NAME.into(),
            ok: false,
            detail: e.to_string(),
        },
    })
}

fn checks_text(checks: &[Check]) -> Rendered {
    let mut lines: Vec<String> = checks
        .iter()
        .map(|check| {
            format!(
                "{} {}: {}",
                if check.ok { "✓" } else { "✗" },
                check.name,
                check.detail
            )
        })
        .collect();
    let failed = checks.iter().filter(|check| !check.ok).count();
    if failed > 0 {
        lines.push(format!("{failed} of {} checks did not pass.", checks.len()));
    }
    Rendered {
        text: lines.join("\n"),
        ok: failed == 0,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actionlog::{Gate, Record};
    use crate::llm::ChatConnect;
    use crate::policy::Class;

    fn logged(home: &Path, tool: &str, gate: Gate, ok: bool) {
        ActionLog::at(home)
            .append(Record {
                session: "s1".into(),
                audience: "terminal".into(),
                tool: tool.into(),
                class: Some(Class::System),
                summary: format!("run: {tool}"),
                reason: "because".into(),
                gate,
                ok,
            })
            .unwrap();
    }

    async fn text(home: &Path, report: Report) -> String {
        render(home, &report, &ChatConnect).await.unwrap().text
    }

    #[tokio::test]
    async fn an_empty_state_directory_reports_plainly() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            text(home.path(), Report::Log { count: 20 }).await,
            "The action log is empty."
        );
        assert_eq!(
            text(home.path(), Report::Sessions).await,
            "No sessions yet."
        );
        assert!(
            text(home.path(), Report::Spend)
                .await
                .starts_with("Today       $0.00\nThis month  $0.00")
        );
        assert!(
            text(home.path(), Report::LogVerify)
                .await
                .starts_with("0 entries")
        );
        assert!(
            render(home.path(), &Report::LogEntry { seq: 1 }, &ChatConnect)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_log_lists_its_latest_entries_and_shows_one_in_full() {
        let home = tempfile::tempdir().unwrap();
        logged(home.path(), "one", Gate::Free, true);
        logged(home.path(), "two", Gate::Approved, true);
        logged(home.path(), "three", Gate::Denied, false);

        let listed = text(home.path(), Report::Log { count: 2 }).await;
        let lines: Vec<&str> = listed.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].trim_start().starts_with("2  ")
                && lines[0].ends_with("approved  system   ok      run: two"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].ends_with("denied    system   failed  run: three"),
            "{}",
            lines[1]
        );

        let shown = text(home.path(), Report::LogEntry { seq: 2 }).await;
        assert!(
            shown.contains("\"gate\": \"approved\"") && shown.contains("\"reason\": \"because\""),
            "{shown}"
        );
        assert!(
            text(home.path(), Report::LogVerify)
                .await
                .starts_with("3 entries")
        );
    }

    #[tokio::test]
    async fn the_doctor_says_what_is_wrong_and_that_is_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        let report = render(home.path(), &Report::Doctor { live: false }, &ChatConnect)
            .await
            .unwrap();
        assert!(!report.ok);
        assert!(
            report.text.contains("✗ connections: none is set up"),
            "{}",
            report.text
        );
        assert!(report.text.contains("checks did not pass."));
        // No worker is configured, so there is no line about one.
        assert!(!report.text.contains("worker"));
    }

    #[tokio::test]
    async fn a_configured_worker_is_started_to_see_that_it_works() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[daemon]\nworker = [\"/nonexistent/tiphys\", \"worker\"]\n",
        )
        .unwrap();
        let report = render(home.path(), &Report::Doctor { live: false }, &ChatConnect)
            .await
            .unwrap();
        assert!(
            report
                .text
                .contains("✗ worker: config: could not start the worker"),
            "{}",
            report.text
        );
    }
}
