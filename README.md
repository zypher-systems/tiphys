# Tiphys

An **always-on agent for your server**. You install it on a machine of its own, and it works there
for you: it runs commands, checks on things, does scheduled jobs and reports back. You reach it from
a terminal app on the server and from chat on your phone.

> **Status: in development, nothing released.** The first milestone (M0) is built: the terminal
> app, a connection set up and checked inside it, reading and changing files, running commands,
> approvals, and the action log. It has been driven end to end against a scripted model; it has not
> yet been run against a real provider. The daemon runs by hand; installing it as a service, and chat, are not built yet.
> See [`ROADMAP.md`](ROADMAP.md) for the order of work, [`design.md`](design.md) for how it fits
> together and [`DECISIONS.md`](DECISIONS.md) for why.

## What it will be

- **A daemon on an Ubuntu server**, running as its own `tiphys` user. The machine is the security
  boundary, so the agent has room to work inside it.
- **A terminal app.** Running `tiphys` opens it. Setup, model connections, keys, chat and approvals
  all happen there. A key is typed into a masked field and never passed on a command line.
- **Chat.** Telegram first, by long polling, so the server opens no inbound port. Only accounts on
  the allowlist are answered.
- **Approvals.** Reads and ordinary changes run. Anything that needs root or changes the system
  asks first, in the app or by a button in chat. A short list of actions is refused everywhere.
- **An action log.** Every tool call leaves a record in a hash-chained file you can read and verify.
- **Scheduled jobs** that run inside a scope you approved and deliver a report to your chat.
- **Plain files.** Config is TOML, memory and skills are Markdown, and everything appended is JSONL.

## Build from source

Nothing is published yet.

```sh
cargo build --release --locked -p tiphys-cli && ./target/release/tiphys
```

Running `tiphys` opens the app. The first time, it asks for a connection: an address that speaks
Chat Completions and its key. It lists the connection's models, makes one real tool call on the one
you choose, and only then saves it.

- `tiphys -p "..."` runs one turn without the app and prints the answer. `-c` carries on the last
  session.
- `tiphys sessions` lists sessions. `tiphys spend` shows what today and this month have cost.
- `tiphys log` lists every tool call and how it came to run or not. `tiphys log verify` checks
  that no entry has been changed or removed.
- `tiphys daemon run` runs the daemon in the foreground. While it runs, the app and `tiphys -p`
  are its clients: close the app in the middle of a turn, open it again, and the turn is there.
  `tiphys daemon status` says whether one is answering.
- `tiphys doctor` checks the installation. `tiphys doctor --live` also makes a real tool call on
  the default connection.

State lives in `~/.tiphys`, or wherever `TIPHYS_HOME` points. [`config.example.toml`](config.example.toml)
shows what can be set by hand.

## Layout

| Crate | Holds |
| --- | --- |
| `crates/tiphys-core` | Config, connections and keys, the provider layer, the agent loop, tools, action policy, action log, sessions |
| `crates/tiphys-tui` | The terminal app |
| `crates/tiphys-cli` | The `tiphys` binary |
| `crates/tiphys-daemon` | The daemon, its socket, chat adapters and the job runner (from M1) |

## License

Apache-2.0. See [`LICENSE`](LICENSE).
