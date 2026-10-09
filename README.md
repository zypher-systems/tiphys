# Tiphys

An **always-on agent for your server**. You install it on a machine of its own, and it works there
for you: it runs commands, checks on things, does scheduled jobs and reports back. You reach it from
a terminal app on the server and from chat on your phone.

> **Status: in development, nothing released.** The first milestone (M0, a first conversation in the
> terminal app) is being built. See [`ROADMAP.md`](ROADMAP.md) for the order of work,
> [`design.md`](design.md) for how it fits together and [`DECISIONS.md`](DECISIONS.md) for why.

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

Nothing is published yet. Once the workspace lands:

```sh
cargo build --release --locked -p tiphys-cli && ./target/release/tiphys
```

## Layout

| Crate | Holds |
| --- | --- |
| `crates/tiphys-core` | Config, connections and keys, the provider layer, the agent loop, tools, action policy, action log, sessions |
| `crates/tiphys-tui` | The terminal app |
| `crates/tiphys-cli` | The `tiphys` binary |
| `crates/tiphys-daemon` | The daemon, its socket, chat adapters and the job runner (from M1) |

## License

Apache-2.0. See [`LICENSE`](LICENSE).
