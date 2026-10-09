# Decisions

Why, not what. Newest first. Each entry: By / Decision / Chosen vs rejected / Why / Where / Residual risk.

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
