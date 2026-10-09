# Tiphys — Design

Status: **proposed** · Started 2026-10-09 · License: Apache-2.0

Tiphys is an always-on agent that is installed on a server and works there for its owner. It is
reached from a terminal app on the server and from chat. It runs commands, checks on things, does
scheduled jobs and reports back, and it asks before anything that changes the system.

This document is the plan. `DECISIONS.md` records why each choice was made; `ROADMAP.md` holds the
order of work.

---

## 1. Principles

1. **The machine is the boundary.** Tiphys is installed on a VM or container made for it, under its
   own user. Approvals are a courtesy and a floor, not containment.
2. **One writer.** The daemon is the only process that writes state. Every surface is its client.
3. **Keys never leak.** A key is entered in the app, in a masked field. It stays out of command
   lines, logs, the action log, transcripts and the environment of every command Tiphys runs.
4. **Fail closed.** Nobody to answer an approval means no. An unknown command is treated as risky.
5. **Every action leaves a record.** Including actions nobody was asked about and actions a
   scheduled job ran. There is no off switch.
6. **Unknown means unknown.** A missing price renders `$?.??`, never `$0.00`. A budget stop stops.
7. **Tool output is data, never instructions.** File contents, command output, web pages and chat
   messages from other people are quoted to the model as untrusted text.
8. **Everything it knows is a plain file.** Config is TOML, memory and skills are Markdown, and
   anything appended is JSONL. You can read, edit or delete all of it.
9. **The prompt prefix is stable.** The system prompt and tool list are fixed for the life of a
   session, so the provider's cache holds.
10. **The code shows the what; `DECISIONS.md` records the why.**

---

## 2. Architecture

```
  terminal app ──┐
  tiphys -p ─────┤  Request / Event          ┌──────────── tiphys daemon (user tiphys) ───────────┐
                 ├──────────────────────────►│ sessions: one actor each, one turn at a time       │
  Telegram ──────┤  one JSON object per line │ agent loop → tools → policy → approval → action log│
  scheduled jobs ┘  over a Unix socket       │ event hub: sequence numbers, replay on reconnect   │
                                             └───────────────────────┬────────────────────────────┘
                                                                     ▼
                                              $TIPHYS_HOME (config, keys, sessions, log, spend)
```

In M0 there is no daemon. The terminal app hosts the agent in its own process and the same
`Request` and `Event` types travel over in-process channels. From M1 they travel over the socket,
and the app does not change.

### 2.1 Crates

| Crate | Kind | Holds | From |
| --- | --- | --- | --- |
| `tiphys-core` | lib | Config, connections, key store, provider layer, spend, agent loop, protocol types, tools, action policy, action log, sessions; later compaction, memory, jobs | M0 |
| `tiphys-tui` | lib | The terminal app: setup, connections and keys, model picker, chat, approval cards | M0 |
| `tiphys-cli` | bin `tiphys` | The app by default; `-p`, `doctor`, `sessions`, `log`, `spend`; later `daemon` and `jobs` | M0 |
| `tiphys-daemon` | lib | Socket server, session actors, event hub, approvals, service install, Telegram adapter, job runner, delivery | M1 |

Edition 2024, `#![forbid(unsafe_code)]`, tokio, reqwest with rustls, ratatui and crossterm. No
provider SDK crate, no database, no HTTP server framework.

### 2.2 State

One directory, `~/.tiphys`, or wherever `TIPHYS_HOME` points. On a server the `tiphys` user's home
is `/var/lib/tiphys`.

| Path | Holds |
| --- | --- |
| `config.toml` | Owner's settings. The app never rewrites it |
| `settings.toml` | What the app saves: connections, the chosen model, UI choices. Layered over `config.toml` |
| `keys/<name>` | One secret per file, mode 0600, directory 0700 |
| `sessions/<id>/` | `meta.json`, `system.md`, `transcript.jsonl`, `events.jsonl` |
| `log/YYYY-MM.jsonl` | The action log, hash-chained |
| `spend/YYYY-MM.jsonl` | What every call cost |
| `tiphys.log` | Diagnostics, rotated |

---

## 3. Connections and keys

A connection is a name, a base URL, a default model and a key.

- Connections are created and edited on the app's setup screen. Nothing else creates one.
- The key is typed into a masked field. Before it is saved the app makes one real request with it
  and shows the result.
- The key is stored in `keys/<connection>`. It is never displayed again; the screen shows only
  whether one is stored.
- No subcommand, flag or argument takes a key.
- A server that must start without anyone present can name an environment variable in
  `config.toml` (`env_key`). The daemon reads it and removes it from the environment of every
  command it runs.
- From M1 the app sends the key to the daemon over the local socket and the daemon stores it. Other
  secrets, such as the Telegram bot token, are entered the same way.

---

## 4. Provider layer

A `Provider` trait: stream one completion, list models.

- Messages, tool specs and stream deltas are Tiphys's own types and do not belong to any wire
  format.
- M0 implements one wire format, Chat Completions, which reaches OpenRouter, OpenAI, local servers
  and most other providers.
- Streaming is parsed incrementally. Tool calls that arrive interleaved are assembled by index.
- Retries with backoff honour `Retry-After`. A stream that stalls is cut and retried.
- Every wire format has recorded fixtures of real streams, and a replay provider feeds them to
  tests. A wire format is not called working until it has passed a live tool-call round trip.
- Prices come from the provider's model list where it has them, with overrides in `config.toml`.
  A model with no known price costs `$?.??` and cannot run unattended while a budget is set.

---

## 5. The agent loop

One `Agent` per session. `turn(input, emit)` runs until the model stops calling tools, and returns
why it stopped.

- The system prompt and tool list are built when the session opens, written to `system.md` and
  reused byte for byte, across turns and restarts.
- Tool calls run in order. Each goes through four steps: **plan** (parse the arguments, classify
  the action, build a preview), **approve**, **run**, **record**.
- A turn stops for: completion, a round cap, the same call with the same result repeated, a reply
  cut off at the output limit several times running, the budget, or a cancel.
- Cancel is checked at every await. A command in flight is killed with its process group.
- Events are passed to a callback. The loop never waits on whoever is listening.

### 5.1 Tools

A `Tool` trait and a registry. Built-in tools, and later MCP tools and external agents, all enter
the same way.

| Tool | Does | From |
| --- | --- | --- |
| `read_file`, `list_dir`, `search_files` | Read | M0 |
| `write_file`, `edit_file` | Change files. The whole new content is worked out before anything is judged, and the owner is shown the diff | M0 |
| `shell` | Run a command with `bash -c`, judged first, with a time limit; whatever it starts is stopped when the call ends | M0 |
| `memory`, `job_save`, `job_delete` | Change what Tiphys keeps | M3 |
| `web_fetch`, `web_search` | Read the web | M4 |

Every tool takes a `reason`: one line, in the model's words, on why it is doing this. The reason is
shown on the approval card and stored in the action log.

---

## 6. Action policy

Every planned action gets one class, and with it a reason when it asks or is refused. The rules are
written for Ubuntu.

| Class | Meaning | Terminal app | Telegram | Scheduled job |
| --- | --- | --- | --- | --- |
| Observe | Reads | Runs | Runs | Runs |
| Change | Changes inside the agent's own home, `/tmp` and `/var/tmp` | Runs | Runs | Only inside the job's scope |
| System | Reaches outside the agent's home, needs root, or touches a file that usually holds a secret | Asks | Asks by button | Only if pre-authorised |
| Never | Reads the key store, changes Tiphys's own state or installation, destroys the machine, stops `tiphys` or `ssh` | Refused | Refused | Refused |

- A path is judged where it really points: symlinks are followed and `..` is worked out first.
- Reading a file that usually holds a secret (a private key, `.env`, `/etc/shadow`) asks, because
  reading it sends it to the model provider. What is already in such a file is not shown in an
  approval preview.
- Tiphys's state directory changes only through Tiphys itself. A tool never writes there.
- A shell command is split into its parts (pipes, lists, redirects, `sudo`, `cd`) and each part is
  classified. The command takes the highest class of its parts.
- A command runs without asking only when all of it is understood: a known program, used in a
  known way, on paths that can be read off the line. A program that is not in the table, a
  variable or a substitution that hides what is acted on, a script, an interpreter, and shell
  syntax beyond a list of commands all ask.
- Nothing on a command line may name the key store, by a path, a pattern that could match it, or a
  recursive read or a git repository that would take it in.
- Change actions run without asking by default, because the machine exists for the agent.
  `[approvals] change = "ask"` makes them ask.
- An approval that nobody answers within 300 seconds is a deny, and the model is told so. In the
  app only `y` is a yes.
- The Never class is checked in core, before any approver is consulted. No surface, setting or
  job can lift it.

### 6.1 Root

Chosen when the service is installed, from M1:

| Setting | Effect |
| --- | --- |
| `none` (default) | `sudo` fails at once. The agent hands the owner the command to run |
| `listed` | Exact commands in `/etc/sudoers.d/tiphys`, generated from the config |
| `all` | Passwordless `sudo`. The approval is the only gate |

Until then Tiphys runs as whoever starts it, and `sudo` works only if that user has it without a
password.

### 6.2 What the rules are not

The rules read the form of an action, not what it does when it runs. A script can do anything,
which is why running one asks, and why the owner should read what they approve. A command the agent
runs is the same user as the agent itself. The rules are a courtesy to the owner and a floor under
the agent; the boundary is the machine, and from M1 the user that commands run as.

---

## 7. The action log

Every tool call appends one entry to `log/YYYY-MM.jsonl`: time, session, audience, tool, class, a
summary of what was asked, the model's reason, how it was let through (free, approved, denied,
refused, or not understood), and whether it worked. Each entry carries the hash of the one before
it, and its own hash covers the rest of it, so a removed or edited entry breaks the chain.
`tiphys log verify` checks it; `tiphys log` lists it.

If the log cannot be written, the action does not run.

The log is evidence for the owner, not a lock. A program running as the Tiphys user could rewrite
the whole file with a new chain. The rules refuse every write to it, and the machine is the
boundary.

---

## 8. Sessions

A session is a directory of append-only files.

- `transcript.jsonl` holds the messages. Compaction appends a marker that says which earlier
  records the model no longer sees; nothing is rewritten, so the full history can always be read.
- `events.jsonl` holds the durable events with sequence numbers. A client that reconnects asks for
  everything after the last number it saw.
- Appends take a lock, are flushed to disk, and a torn last line is repaired on open.
- A session belongs to an audience: a terminal user, or one chat. Memory is scoped the same way and
  is never shared across audiences.

---

## 9. The daemon (M1)

`tiphys daemon run` is the agent's hosts behind a Unix socket. It is the only process that writes
the state directory, and it holds a lock on it; an app started by itself on the same directory is
refused.

- **Audiences.** There is one host per audience, and each has its own conversation: `terminal` for
  the owner at the app, `oneshot` for `tiphys -p`. Chat adapters will be audiences too.
- **Clients.** A client connects, says which audience it is talking as, and is attached to that
  host. It is sent where things stand and the conversation so far, then everything that happens.
  When it leaves, the host and any turn it is running carry on.
- **Catching up.** A client can attach in the middle of a turn. The host reads the session's
  events and adds the client between two steps of the turn, so nothing is missed and nothing comes
  twice. A question still waiting for an answer is shown to it, and it can answer.
- **Who gets what.** What happens in a turn goes to every client of the audience. The answer to a
  client's own question, such as a model list, goes to that client alone.
- **The socket.** `/run/tiphys/tiphys.sock` for an installed service, beside the state directory
  when the daemon is run by hand, or wherever `TIPHYS_SOCKET` says. It is open to its owner and its
  group. The daemon also asks the kernel who is on the other end: its own user, root, or an owner
  listed in `[daemon] owners`.
- **The wire.** One JSON object per line. A client opens with a hello carrying the protocol
  version; a daemon and a client that differ do not try to understand each other.
- **Restarts.** A host picks up its audience's last session. A session whose events end in the
  middle of a turn was cut short; it is closed when it is opened, so no client waits on it.
- **Stopping.** On SIGTERM the daemon stops any running turn the same way a cancel does, so the
  end of the turn is on the record, then removes its socket.

A typed key is the one secret that crosses the socket, from the app to the daemon, which stores
it. Nothing sends one the other way.

---

## 10. Milestones

| M | Deliverable |
| --- | --- |
| M0 | The terminal app with the agent in-process: setup and key entry, chat, file and shell tools, approvals, the action log, spend |
| M1 | The daemon and its socket, the app as a client, a system unit, the installer for Ubuntu |
| M2 | Telegram, an allowlist, approvals in chat, compaction |
| M3 | Memory scoped per chat; scheduled jobs with delivery |
| M4 | `web_fetch`, `web_search` |
| M5 | An identity file, skills, memory search, group chats |

What each is checked by is in `ROADMAP.md`.
