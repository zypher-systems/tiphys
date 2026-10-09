# Decisions

Why, not what. Newest first. Each entry: By / Decision / Chosen vs rejected / Why / Where / Residual risk.

### 2026-10-09: The daemon catches a client up from inside the host, with no numbers on the wire
- **By:** design, while building the daemon.
- **Decision:**
  - A client attaches to a host by a message in the host's own inbox. The host answers it between two steps of whatever it is doing: it sends the state and the session's events as one `history` event, then adds the client to those it sends to.
  - There is one host per audience. `terminal` and `oneshot` are separate conversations; a client says which it is talking as.
  - Turn events go to every client of the audience. Replies to a client's own request go to that client.
  - A client leaving stops nothing. The daemon stopping cancels the running turn.
  - With no daemon running, the app hosts the agent itself, under the same lock a daemon takes. With one running, the app and `tiphys -p` are clients.
- **Chosen vs rejected:**
  - Rejected sequence numbers on every event with a client asking for "everything after N": a client that reconnects is a new process with nothing on its screen, so it always needs the conversation, and the host can hand it over without a race because it writes and sends each event in one step. The numbers are still in the session's event file.
  - Rejected one conversation shared by the app and one-shot runs: a script calling `tiphys -p` would write into what the owner is reading.
  - Rejected stopping a turn when its last client leaves: the point of the daemon is that a dropped connection costs nothing.
  - Rejected making the app refuse to run without a daemon: on a workstation that is one more thing to start.
- **Why:** The milestone is "drop the connection in the middle of a turn, come back, and it is all there". That needs the turn to outlive the client and the client to be caught up exactly.
- **Where:** `tiphys-core/src/host.rs` (`Input`, `attach`, `turn`), `wire.rs`, `client.rs`, `lock.rs`; `tiphys-daemon/src/lib.rs`.
- **Residual risk:**
  - The text of a reply that was streaming when a client attached is not replayed; the client sees the reply from where it joined, and the whole message when it is finished.
  - A client is sent at most the last 400 events of a session.
  - Anything running as an owner's user is that owner to the daemon.
  - A Unix socket's path can be 107 bytes at most. A state directory deep in the filesystem needs `TIPHYS_SOCKET`.

### 2026-10-09: A shell command runs unasked only when all of it is understood
- **By:** design, while building the shell tool.
- **Decision:**
  - A command line is split at `;`, `&&`, `||`, `|` and `&`. Each part is matched to a table of programs found on an Ubuntu server, and the paths it names go through the path rules. The command takes the strictest verdict of its parts.
  - Asking is the default. A command leaves it only when every part is a known program, used in a known way, on paths written out on the line. An unknown program, an interpreter or a script, `xargs`, a variable other than `$HOME`, a command substitution, a here-document, a subshell, a loop: all ask.
  - Nothing on the line may name the key store: a path into it, a pattern that could match its way there, a recursive read or a `find -exec` over a directory that holds it, a link to the state directory or above it, a git repository at or above it. These are refused, not asked about.
  - Refused as well: `rm -r`, `mv`, `chmod -R` and `chown -R` on a system directory, the agent's home or its state; anything that formats or writes over a disk; stopping `tiphys` or `ssh`; deleting the `tiphys` user.
  - A tree copied or moved, or an archive unpacked, straight into a directory that holds the state asks, because what is in it cannot be seen.
  - A change under `~/.ssh` asks even where the file is no secret, since it decides who can log in.
  - Paths through `/proc/<pid>/` into another process ask.
  - The command runs with `bash -c`, no terminal and nothing on standard input, with a time limit. It is its own process group, and the group is killed when the call ends for any reason, so nothing outlives a cancelled turn. Variables that look like secrets are left out of its environment.
- **Chosen vs rejected:**
  - Rejected a list of dangerous patterns matched against the text: a deny-list fails open on whatever nobody thought of. An allow-table fails toward asking.
  - Rejected letting scripts and interpreters run unasked on a machine made for the agent: one line of `python3 -c` can do everything the rules exist to notice.
  - Rejected asking, in place of refusing, for a recursive read over the key store: an approval card that says "it would read through the key store" will be approved by someone in a hurry, and then the key is in a prompt.
  - Rejected leaving background processes running after a call: a turn that was cancelled must leave nothing behind it.
- **Why:** The owner should be asked about what matters and nothing else, and the rule for telling the two apart has to be one that is wrong in the safe direction.
- **Where:** `tiphys-core/src/policy/shell.rs`, `tools/shell.rs`.
- **Residual risk:**
  - The rules read a command's form. `make`, a script, or any program not in the table asks, and what happens after the owner says yes is whatever it does.
  - A command runs as the same user as the agent, so the key file is readable to it. The rules catch the ways of naming it that are in the tests; they are not a boundary. M1 runs commands where the key store cannot be read.
  - `curl` to any address runs unasked as long as it sends no data, so the agent can reach anything the machine can reach.
  - The agent cannot start a long-running process through the shell, because the process group dies with the call.
  - The table will be wrong in places. A command that asks and should not is an annoyance; one that runs and should have asked is a defect, and belongs in the tests when found.

### 2026-10-09: The agent's home is its own; a write anywhere else asks
- **By:** design, while building the file tools.
- **Decision:**
  - A write is Change inside the Tiphys user's home, `/tmp` and `/var/tmp`, and System everywhere else. The System class is defined by reach, not by a list of system directories.
  - Tiphys's state directory, its binary, its unit and its sudoers file are Never to write. Its state changes only through its own code.
  - A file that usually holds a secret asks before it is read or written, judged from its name and place alone. The key store is Never to read.
  - A path is resolved before it is judged, so a rule about a place holds through symlinks and `..`.
  - `write_file` and `edit_file` work out the whole new content when they are planned. The owner approves a diff of exactly what will be written, and a file that changed while the question was open is left alone.
  - In the app only `y` approves. Enter, other keys and a paste do nothing.
- **Chosen vs rejected:**
  - Rejected listing system directories (`/etc`, `/usr`, ...) as the System class: a list has gaps, and "outside my own home" has none.
  - Rejected letting a tool write Tiphys's config, even with approval: text from a file or a web page could talk the model into changing the rules it runs under, and the owner would be approving a diff, not a policy.
  - Rejected showing a secret file's contents in the approval card: the card is an event, and events are kept with the session.
  - Rejected Enter as yes: it is the key most often pressed by accident.
- **Why:** The first version of the plan called the third class "changes the system". Writing the rules showed that the useful line is the edge of the agent's own home.
- **Where:** `tiphys-core/src/policy/paths.rs`, `tools/write.rs`, `approval.rs`, `agent.rs` (`judge_and_run`); the card in `tiphys-tui/src/draw.rs`.
- **Residual risk:**
  - A secret is recognised by name. A key in a file called `notes.txt` is read without asking.
  - What the model itself writes into a secret file is in the transcript, as everything the model says is.
  - Reads outside home run without asking, so the model provider sees whatever the Tiphys user can read that is not named like a secret.

### 2026-10-09: One agent per session, no orchestrator
- **By:** design, at the start.
- **Decision:** A session has one agent. A scheduled job is a new agent with no history. Handing work to another agent, when it comes, is one tool that runs an external agent CLI and returns its answer.
- **Chosen vs rejected:** Rejected an orchestrator with planner and worker agents: it multiplies cost and failure modes before a single agent has been proven on real work. Rejected a dispatch tool tied to one agent product: the owner wants any agent CLI to fit.
- **Why:** The first jobs (chat, scheduled reports, web errands) need one loop that finishes what it starts.
- **Where:** `tiphys-core/src/agent.rs`; later a single dispatch tool under `tools/`.
- **Residual risk:** Long jobs run in one context and lean on compaction.

### 2026-10-09: Chat Completions first, behind a wire-neutral provider trait
- **By:** the owner.
- **Decision:** The first version speaks one wire format, Chat Completions. Messages, tool specs and stream deltas are Tiphys's own types, so further formats are additions.
- **Chosen vs rejected:** Rejected building three formats at once: each needs its own fixtures and a live check per release, and one format already reaches OpenRouter, OpenAI and local servers. Rejected a provider SDK crate: it hides the stream handling that most needs testing.
- **Why:** Start small; prove one format properly.
- **Where:** `tiphys-core/src/llm/`.
- **Residual risk:** Provider features that only exist on a native format, such as explicit cache breakpoints, wait for that format.

### 2026-10-09: Sessions are append-only and the prompt is frozen per session
- **By:** design, at the start.
- **Decision:** The system prompt and tool list are built when a session opens, stored as `system.md` and reused unchanged. Transcripts and events are only appended. Compaction appends a marker; it never rewrites a file.
- **Chosen vs rejected:** Rejected rebuilding the prompt each turn: every change throws away the provider's cached prefix, and a chat session that never resets pays for that on every message. Rejected rewriting the transcript on compaction: the full history is the only source a later search index can be rebuilt from.
- **Why:** Chat sessions run for weeks. Cost and recoverability both depend on a stable prefix and a history nothing has edited.
- **Where:** `tiphys-core/src/session.rs`, `prompt.rs`, `jsonl.rs`.
- **Residual risk:** Memory written during a session reaches the prompt only at the next session or compaction.

### 2026-10-09: Four action classes, answered differently by each surface
- **By:** design, at the start.
- **Decision:** Every action is Observe, Change, System or Never. Observe and Change run. System asks, in the app or by a button in chat, and is denied after 300 seconds without an answer. Never is refused before any approver is asked. Root is an install-time choice: none (default), listed commands, or all.
- **Chosen vs rejected:** Rejected asking for every change: on a machine made for the agent that is noise, and noise trains the owner to approve without reading. Rejected letting chat lift the Never class: a stolen chat account must not be able to destroy the machine. Rejected passwordless root by default: the owner opts in.
- **Why:** The machine is the boundary, so the policy's job is to keep the owner informed and to hold a small floor.
- **Where:** `tiphys-core/src/policy/`, `approval.rs`; `design.md` §6.
- **Residual risk:** The classifier reads the form of a command, not what a script does. With root set to `all`, the approval is the only gate.

### 2026-10-09: The daemon is the only writer; every surface is a client on a socket
- **By:** design, at the start.
- **Decision:** From M1 the daemon owns all state. The terminal app, one-shot runs, Telegram and scheduled jobs reach it through one set of `Request` and `Event` types, one JSON object per line over a Unix socket in `/run/tiphys`. In M0 the same types travel over in-process channels.
- **Chosen vs rejected:** Rejected several processes sharing the state directory: concurrent writers to one store are the classic source of corruption in agents of this kind. Rejected coordinating through files alone: a client must see a turn stream live and send an approval back. Rejected an HTTP API: a local socket needs no port, no TLS and no framework.
- **Why:** A session can be driven from the terminal and from chat at the same time. One writer makes that safe.
- **Where:** `tiphys-core/src/proto.rs`; `tiphys-daemon` from M1.
- **Residual risk:** Any process running as an owner's login user is that owner to the daemon.

### 2026-10-09: Ubuntu servers are the target
- **By:** the owner.
- **Decision:** Tiphys is built for Ubuntu servers: a dedicated VM or LXC, a system service under its own `tiphys` user. The policy rules, the unit and the installer are written for Ubuntu.
- **Chosen vs rejected:** Rejected a distribution layer that covers several families from the start: each family's package manager needs its own rules and its own proof, and one target can be done well.
- **Why:** One supported target keeps the policy rules and the install path testable.
- **Where:** `tiphys-core/src/policy/shell.rs`; packaging from M1.
- **Residual risk:** On another distribution the binary runs, but package commands the rules do not know are classed System and ask every time.

### 2026-10-09: Keys are entered in the app, never on a command line
- **By:** the owner.
- **Decision:** A key or token is typed into a masked field in the terminal app, tested, and stored in `keys/<name>` at mode 0600. No subcommand, flag or argument takes a secret. A headless server may name an environment variable in `config.toml`.
- **Chosen vs rejected:** Rejected a `key set` subcommand: a secret on a command line lands in shell history and the process list, and it is a second way in that has to be kept as safe as the first.
- **Why:** One place to enter a secret, and that place can test it before saving.
- **Where:** `tiphys-core/src/keys.rs`; the setup screen in `tiphys-tui`.
- **Residual risk:** The key file is readable by anything running as the `tiphys` user, which includes commands the agent runs. The policy refuses reads of `keys/`, and the machine is the boundary.
