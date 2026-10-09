//! Installing the daemon as a service on an Ubuntu server.
//!
//! `tiphys daemon install --owner <name>` makes Tiphys two users and a
//! service:
//!
//! | What | Where |
//! | --- | --- |
//! | The daemon's user, who holds the keys and the state | `tiphysd`, home `/var/lib/tiphysd`, mode 0700 |
//! | The user the agent acts as | `tiphys`, home `/home/tiphys` |
//! | The one thing the first may do as the second | `/etc/sudoers.d/tiphys` |
//! | The service | `/etc/systemd/system/tiphys.service` |
//! | Who may talk to it, and how it starts its worker | `[daemon]` in `/var/lib/tiphysd/settings.toml` |
//!
//! The owner is added to the `tiphysd` group, which is what lets them reach
//! the socket in `/run/tiphys`.
//!
//! The install is worked out as a list of steps before anything is done.
//! `--dry-run` prints the list, and the tests check it, so what root will do
//! can be read without being root. Running it again is how an update is
//! applied: every step is safe to repeat, and nothing in the state directory
//! but the `[daemon]` settings is touched.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tiphys_core::{Error, Result, settings};

/// The user the daemon runs as.
pub const DAEMON_USER: &str = "tiphysd";
/// The user the agent's actions run as.
pub const WORK_USER: &str = "tiphys";
/// The daemon's state directory, which is its user's home.
pub const STATE_DIR: &str = "/var/lib/tiphysd";
/// The home the agent works in.
pub const WORK_HOME: &str = "/home/tiphys";
pub const UNIT_PATH: &str = "/etc/systemd/system/tiphys.service";
pub const SUDOERS_PATH: &str = "/etc/sudoers.d/tiphys";
const UNIT: &str = "tiphys.service";

/// What an install is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The user who will talk to Tiphys.
    pub owner: String,
    /// Where the `tiphys` binary is.
    pub binary: PathBuf,
}

/// What the install needs to know about the machine.
pub trait Machine {
    /// The numeric id of a user, if there is such a user.
    fn uid(&self, user: &str) -> Option<u32>;

    /// Whether a file belongs to root and can be changed by nobody else.
    fn only_root_can_change(&self, path: &Path) -> bool;
}

/// One thing the install does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a command.
    Run { why: String, argv: Vec<String> },
    /// Write a file owned by root. With `check`, the content is first written
    /// beside the file and that command is run on it; the file is only put in
    /// place if the command succeeds.
    Write {
        why: String,
        path: PathBuf,
        content: String,
        mode: u32,
        check: Option<Vec<String>>,
    },
    /// Set who may talk to the daemon and how it starts its worker, in the
    /// daemon's settings.
    Settings {
        state: PathBuf,
        owner: u32,
        worker: Vec<String>,
    },
}

fn run(why: &str, argv: &[&str]) -> Step {
    Step::Run {
        why: why.into(),
        argv: argv.iter().map(|part| (*part).to_string()).collect(),
    }
}

/// The command that starts the worker as the work user.
pub fn worker_command(binary: &Path) -> Vec<String> {
    ["sudo", "-n", "-H", "-u", WORK_USER]
        .into_iter()
        .map(String::from)
        .chain([binary.to_string_lossy().into_owned(), "worker".into()])
        .collect()
}

/// The sudoers file: the daemon's user may start the worker as the work
/// user, with no password, and may do nothing else.
pub fn sudoers_text(binary: &Path) -> String {
    format!(
        "# Written by `tiphys daemon install`. Lets the Tiphys daemon start its worker\n\
         # as the user the agent acts as. It allows nothing else.\n\
         {DAEMON_USER} ALL=({WORK_USER}) NOPASSWD: {} worker\n",
        binary.display()
    )
}

/// The systemd unit.
pub fn unit_text(binary: &Path) -> String {
    format!(
        "# Written by `tiphys daemon install`.\n\
         [Unit]\n\
         Description=Tiphys agent daemon\n\
         Documentation=https://github.com/zypher-systems/tiphys\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         User={DAEMON_USER}\n\
         Group={DAEMON_USER}\n\
         ExecStart={binary} daemon run\n\
         Environment=TIPHYS_HOME={STATE_DIR}\n\
         WorkingDirectory={STATE_DIR}\n\
         # /run/tiphys, where the socket is. The owner reaches it through the\n\
         # {DAEMON_USER} group.\n\
         RuntimeDirectory=tiphys\n\
         RuntimeDirectoryMode=0750\n\
         # Everything the daemon writes is its own alone.\n\
         UMask=0077\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # A bound on what a runaway command can start.\n\
         TasksMax=4096\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        binary = binary.display()
    )
}

/// Works out what an install will do on this machine.
pub fn plan(options: &Options, machine: &dyn Machine) -> Result<Vec<Step>> {
    let owner = machine.uid(&options.owner).ok_or_else(|| {
        Error::Config(format!(
            "there is no user named `{}` on this machine",
            options.owner
        ))
    })?;
    if !options.binary.is_absolute() {
        return Err(Error::Config(format!(
            "the binary's path, {}, is not absolute",
            options.binary.display()
        )));
    }
    let binary = options.binary.to_string_lossy();
    if binary.contains(char::is_whitespace) || binary.contains([',', ':', '=', '\\']) {
        return Err(Error::Config(format!(
            "the binary's path, {binary}, has characters a sudoers file cannot carry"
        )));
    }
    // The service runs this file, and the sudoers rule lets it be run as the
    // work user. If anyone but root could replace it, they could do both.
    if !machine.only_root_can_change(&options.binary) {
        return Err(Error::Config(format!(
            "{binary} can be changed by a user other than root, so it cannot be what the service \
             runs; install Tiphys to /usr/local/bin first, which install.sh does"
        )));
    }
    let mut steps = Vec::new();
    if machine.uid(DAEMON_USER).is_none() {
        steps.push(run(
            "create the user the daemon runs as",
            &[
                "useradd",
                "--system",
                "--user-group",
                "--home-dir",
                STATE_DIR,
                "--shell",
                "/usr/sbin/nologin",
                DAEMON_USER,
            ],
        ));
    }
    if machine.uid(WORK_USER).is_none() {
        steps.push(run(
            "create the user the agent acts as, with a home to work in",
            &[
                "useradd",
                "--system",
                "--user-group",
                "--create-home",
                "--home-dir",
                WORK_HOME,
                "--shell",
                "/bin/bash",
                WORK_USER,
            ],
        ));
    }
    steps.push(run(
        "let the owner reach the daemon's socket",
        &[
            "usermod",
            "--append",
            "--groups",
            DAEMON_USER,
            &options.owner,
        ],
    ));
    steps.push(run(
        "make the state directory, which only the daemon's user can enter",
        &[
            "install",
            "--directory",
            "--mode",
            "0700",
            "--owner",
            DAEMON_USER,
            "--group",
            DAEMON_USER,
            STATE_DIR,
        ],
    ));
    steps.push(Step::Settings {
        state: STATE_DIR.into(),
        owner,
        worker: worker_command(&options.binary),
    });
    steps.push(run(
        "hand the settings to the daemon's user",
        &[
            "chown",
            &format!("{DAEMON_USER}:{DAEMON_USER}"),
            &format!("{STATE_DIR}/settings.toml"),
        ],
    ));
    steps.push(Step::Write {
        why: "let the daemon start its worker as the other user, and nothing else".into(),
        path: SUDOERS_PATH.into(),
        content: sudoers_text(&options.binary),
        mode: 0o440,
        check: Some(vec![
            "visudo".into(),
            "--check".into(),
            "--quiet".into(),
            "--file".into(),
        ]),
    });
    steps.push(Step::Write {
        why: "the service".into(),
        path: UNIT_PATH.into(),
        content: unit_text(&options.binary),
        mode: 0o644,
        check: None,
    });
    steps.push(run(
        "have systemd read the service",
        &["systemctl", "daemon-reload"],
    ));
    steps.push(run(
        "start the service at boot",
        &["systemctl", "enable", UNIT],
    ));
    steps.push(run(
        "start it now, or start the new version",
        &["systemctl", "restart", UNIT],
    ));
    Ok(steps)
}

/// Works out what removing the service does. The users and the data stay.
pub fn uninstall_plan() -> Vec<Step> {
    vec![
        run(
            "stop the service and keep it from starting",
            &["systemctl", "disable", "--now", UNIT],
        ),
        run("remove the service", &["rm", "--force", UNIT_PATH]),
        run(
            "remove what let the daemon start its worker",
            &["rm", "--force", SUDOERS_PATH],
        ),
        run(
            "have systemd forget the service",
            &["systemctl", "daemon-reload"],
        ),
    ]
}

/// A step as the owner reads it in a dry run.
pub fn describe(step: &Step) -> String {
    match step {
        Step::Run { why, argv } => format!("{why}:\n    {}", argv.join(" ")),
        Step::Write {
            why,
            path,
            content,
            mode,
            check,
        } => {
            let checked = match check {
                Some(check) => format!(", checked with `{} <file>`", check.join(" ")),
                None => String::new(),
            };
            let body: String = content
                .lines()
                .map(|line| format!("    | {line}\n"))
                .collect();
            format!(
                "{why}:\n    write {} (mode {mode:04o}{checked})\n{}",
                path.display(),
                body.trim_end()
            )
        }
        Step::Settings {
            state,
            owner,
            worker,
        } => format!(
            "say who may talk to the daemon and how it starts its worker:\n    in {}/settings.toml, [daemon] owners gains {owner} and worker = {worker:?}",
            state.display()
        ),
    }
}

/// Does the steps, in order, stopping at the first that fails.
pub fn apply(steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Run { why, argv } => {
                command(argv).map_err(|e| Error::Config(format!("could not {why}: {e}")))?
            }
            Step::Write {
                why,
                path,
                content,
                mode,
                check,
            } => {
                write_checked(path, content, *mode, check.as_deref()).map_err(|e| {
                    Error::Config(format!("could not write {} ({why}): {e}", path.display()))
                })?;
            }
            Step::Settings {
                state,
                owner,
                worker,
            } => settings::set_daemon(state, *owner, worker)?,
        }
    }
    Ok(())
}

fn command(argv: &[String]) -> std::result::Result<(), String> {
    let (program, args) = argv.split_first().ok_or("an empty command")?;
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("`{program}` did not start: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed with {status}", argv.join(" ")))
    }
}

/// Writes `content` beside `path`, checks it if there is a check, and moves
/// it into place. A file that fails its check never has the real name.
fn write_checked(
    path: &Path,
    content: &str,
    mode: u32,
    check: Option<&[String]>,
) -> std::result::Result<(), String> {
    // A name with a dot in it is one sudo does not read from its directory.
    let mut draft = path.as_os_str().to_owned();
    draft.push(".new");
    let draft = PathBuf::from(draft);
    let io = |e: std::io::Error| e.to_string();
    std::fs::write(&draft, content).map_err(io)?;
    std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(mode)).map_err(io)?;
    if let Some(check) = check {
        let mut argv = check.to_vec();
        argv.push(draft.to_string_lossy().into_owned());
        if let Err(e) = command(&argv) {
            let _ = std::fs::remove_file(&draft);
            return Err(e);
        }
    }
    std::fs::rename(&draft, path).map_err(io)
}

/// The machine this is running on.
pub struct ThisMachine;

impl Machine for ThisMachine {
    fn uid(&self, user: &str) -> Option<u32> {
        // `id` asks the system's own user database, wherever users are kept.
        let output = Command::new("id").args(["-u", "--", user]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }

    fn only_root_can_change(&self, path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        // The file, and every directory above it: whoever can rename a
        // directory on the way can swap what is at the end of it.
        path.ancestors().all(|part| {
            part.as_os_str().is_empty()
                || std::fs::metadata(part).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake(HashMap<&'static str, u32>);

    impl Machine for Fake {
        fn uid(&self, user: &str) -> Option<u32> {
            self.0.get(user).copied()
        }
        fn only_root_can_change(&self, path: &Path) -> bool {
            path.starts_with("/usr")
        }
    }

    fn options() -> Options {
        Options {
            owner: "ada".into(),
            binary: "/usr/local/bin/tiphys".into(),
        }
    }

    fn commands(steps: &[Step]) -> Vec<String> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv.join(" ")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_first_install_makes_two_users_a_rule_between_them_and_a_service() {
        let fresh = Fake(HashMap::from([("ada", 1000)]));
        let steps = plan(&options(), &fresh).unwrap();
        assert_eq!(
            commands(&steps),
            [
                "useradd --system --user-group --home-dir /var/lib/tiphysd --shell /usr/sbin/nologin tiphysd",
                "useradd --system --user-group --create-home --home-dir /home/tiphys --shell /bin/bash tiphys",
                "usermod --append --groups tiphysd ada",
                "install --directory --mode 0700 --owner tiphysd --group tiphysd /var/lib/tiphysd",
                "chown tiphysd:tiphysd /var/lib/tiphysd/settings.toml",
                "systemctl daemon-reload",
                "systemctl enable tiphys.service",
                "systemctl restart tiphys.service",
            ]
        );
        let settings = steps.iter().find_map(|step| match step {
            Step::Settings {
                state,
                owner,
                worker,
            } => Some((state.clone(), *owner, worker.join(" "))),
            _ => None,
        });
        assert_eq!(
            settings,
            Some((
                "/var/lib/tiphysd".into(),
                1000,
                "sudo -n -H -u tiphys /usr/local/bin/tiphys worker".into()
            ))
        );
        // The users exist before anything is given to them, and the service
        // starts last.
        let written: Vec<&Path> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Write { path, .. } => Some(path.as_path()),
                _ => None,
            })
            .collect();
        assert_eq!(written, [Path::new(SUDOERS_PATH), Path::new(UNIT_PATH)]);
        assert!(matches!(steps.last(), Some(Step::Run { argv, .. }) if argv[1] == "restart"));
    }

    #[test]
    fn an_update_makes_no_users_and_repeats_the_rest() {
        let installed = Fake(HashMap::from([
            ("ada", 1000),
            ("tiphysd", 998),
            ("tiphys", 997),
        ]));
        let steps = plan(&options(), &installed).unwrap();
        assert!(
            !commands(&steps)
                .iter()
                .any(|command| command.starts_with("useradd"))
        );
        assert_eq!(
            steps.len(),
            plan(&options(), &Fake(HashMap::from([("ada", 1000)])))
                .unwrap()
                .len()
                - 2
        );
    }

    #[test]
    fn the_sudoers_file_allows_one_command_as_one_user() {
        let text = sudoers_text(Path::new("/usr/local/bin/tiphys"));
        let rules: Vec<&str> = text.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(
            rules,
            ["tiphysd ALL=(tiphys) NOPASSWD: /usr/local/bin/tiphys worker"]
        );
        // It is exactly the command the daemon is told to run.
        let command = worker_command(Path::new("/usr/local/bin/tiphys")).join(" ");
        assert_eq!(command, "sudo -n -H -u tiphys /usr/local/bin/tiphys worker");
        assert!(rules[0].ends_with(command.split_once("tiphys ").unwrap().1));
        // And it is checked before it is ever put where sudo reads it.
        let steps = plan(&options(), &Fake(HashMap::from([("ada", 1000)]))).unwrap();
        let checked = steps.iter().any(|step| {
            matches!(step, Step::Write { path, check: Some(check), mode: 0o440, .. }
                if path == Path::new(SUDOERS_PATH) && check[0] == "visudo")
        });
        assert!(checked);
    }

    #[test]
    fn the_unit_runs_the_daemon_as_its_own_user_with_its_state_and_socket() {
        let unit = unit_text(Path::new("/usr/local/bin/tiphys"));
        for line in [
            "User=tiphysd",
            "ExecStart=/usr/local/bin/tiphys daemon run",
            "Environment=TIPHYS_HOME=/var/lib/tiphysd",
            "RuntimeDirectory=tiphys",
            "RuntimeDirectoryMode=0750",
            "UMask=0077",
            "Restart=on-failure",
            "WantedBy=multi-user.target",
        ] {
            assert!(
                unit.lines().any(|l| l == line),
                "missing {line:?} in:\n{unit}"
            );
        }
        // The daemon has to be able to use sudo for its worker, which these
        // would take away.
        for directive in [
            "NoNewPrivileges",
            "ProtectSystem",
            "RestrictSUIDSGID",
            "PrivateUsers",
        ] {
            assert!(
                !unit.contains(directive),
                "{directive} would stop the worker from starting"
            );
        }
    }

    #[test]
    fn an_install_that_cannot_work_is_refused_before_anything_is_done() {
        let nobody = Fake(HashMap::new());
        let err = plan(&options(), &nobody).unwrap_err().to_string();
        assert!(err.contains("no user named `ada`"), "{err}");

        let known = Fake(HashMap::from([("ada", 1000)]));
        // A binary its owner could replace must not become what root's
        // service runs.
        let in_home = Options {
            binary: "/home/ada/tiphys/target/release/tiphys".into(),
            ..options()
        };
        let err = plan(&in_home, &known).unwrap_err().to_string();
        assert!(
            err.contains("can be changed by a user other than root"),
            "{err}"
        );
        for binary in ["tiphys", "/usr/my apps/tiphys", "/usr/a,b/tiphys"] {
            let bad = Options {
                binary: binary.into(),
                ..options()
            };
            assert!(plan(&bad, &known).is_err(), "{binary}");
        }
    }

    #[test]
    fn uninstalling_stops_and_removes_the_service_and_leaves_users_and_data() {
        let commands = commands(&uninstall_plan());
        assert_eq!(
            commands,
            [
                "systemctl disable --now tiphys.service",
                "rm --force /etc/systemd/system/tiphys.service",
                "rm --force /etc/sudoers.d/tiphys",
                "systemctl daemon-reload",
            ]
        );
        assert!(
            !commands
                .iter()
                .any(|c| c.contains("userdel") || c.contains("/var/lib") || c.contains("/home"))
        );
    }

    #[test]
    fn a_file_is_only_put_in_place_once_its_check_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiphys");
        std::fs::write(&path, "the old rule\n").unwrap();

        let failing = ["false".to_string()];
        assert!(write_checked(&path, "a broken rule\n", 0o440, Some(&failing)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the old rule\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let passing = ["true".to_string()];
        write_checked(&path, "the new rule\n", 0o440, Some(&passing)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the new rule\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o440
        );
    }

    #[test]
    fn a_dry_run_shows_every_command_and_every_file_in_full() {
        let steps = plan(&options(), &Fake(HashMap::from([("ada", 1000)]))).unwrap();
        let shown: Vec<String> = steps.iter().map(describe).collect();
        let all = shown.join("\n");
        assert!(all.contains("useradd --system --user-group --home-dir /var/lib/tiphysd"));
        assert!(all.contains("    | tiphysd ALL=(tiphys) NOPASSWD: /usr/local/bin/tiphys worker"));
        assert!(all.contains("    | ExecStart=/usr/local/bin/tiphys daemon run"));
        assert!(all.contains("checked with `visudo --check --quiet --file <file>`"));
        assert!(all.contains("[daemon] owners gains 1000"));
    }
}
