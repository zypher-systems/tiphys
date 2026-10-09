# Tiphys

An **always-on agent for your server**. You install it on a machine of its own, and it works there
for you: it runs commands, checks on things, does scheduled jobs and reports back. You reach it from
a terminal app on the server and from chat on your phone.

> **Status: 0.2.0, early.** Tiphys runs as a service on an Ubuntu server and is reached from a
> terminal app and from a Telegram chat: it reads and changes files and runs commands, asks before
> anything that reaches beyond its own home, and keeps a record of every action. Scheduled jobs,
> memory and web tools are not built yet. See [`ROADMAP.md`](ROADMAP.md) for the order of work, [`design.md`](design.md) for how
> it fits together and [`DECISIONS.md`](DECISIONS.md) for why.

## Install

On an Ubuntu server, as a user who can use `sudo`:

```sh
curl -fsSL https://raw.githubusercontent.com/zypher-systems/tiphys/main/install.sh | sh
```

This downloads the release for the machine, checks it against the release's checksums, puts the
binary in `/usr/local/bin`, and sets Tiphys up as a service. `tiphys daemon install --dry-run`
shows every command and file that setup involves. Running the installer again updates Tiphys; your
data is not touched. `--uninstall` removes the service and the binary and keeps the data.

An installed Tiphys is two users:

| User | Does | Home |
| --- | --- | --- |
| `tiphysd` | Runs the daemon. Holds the keys, the sessions and the action log | `/var/lib/tiphysd`, which nobody else can enter |
| `tiphys` | Is who the agent acts as: every file it reads or writes, every command it runs | `/home/tiphys` |

A command the agent runs cannot read a key, because the user it runs as cannot.

You are added to two groups: `tiphysd`, which is what lets you reach the daemon, and `tiphys`,
which is what lets you read the files the agent writes in `/home/tiphys`. Log out and in again,
then:

```sh
tiphys
```

The first time, the app asks for a connection: an address that speaks Chat Completions (OpenRouter,
OpenAI, a server of your own) and its key. The key is typed into a masked field and never passed on
a command line. The app lists the connection's models, makes one real tool call on the one you
choose, and only then saves it.

## Using it

- **`tiphys`** opens the app. Close it in the middle of a turn, or lose the connection, and the
  turn carries on; open it again and the conversation is there. `/new` starts a fresh session,
  `/model` picks another model, `/connections` changes the connection, `/help` lists the rest.
- **Approvals.** Looking around runs at once, and so do changes inside the agent's own home.
  Anything else shows a card first: `sudo`, packages and services, writing outside its home, files
  that usually hold a secret, scripts and programs whose effects cannot be read off the command
  line. Only `y` approves. A short list of things is refused whoever asks.
- **Telegram.** `/telegram` in the app sets up a chat bot. Ask @BotFather in Telegram for a bot,
  paste its token into the masked field, then press `p`: the app shows a code, you send it to the
  bot from your own Telegram account, and the app asks whether that was you. From then on the bot
  answers you, in a conversation of its own, and nobody else: anyone not paired gets no reply at
  all. What has to ask arrives with Approve and Deny buttons. `/new` starts a fresh conversation
  and `/stop` stops what is running. The server opens no port for any of this.
- **`tiphys -p "..."`** runs one turn without the app and prints the answer. Nobody is there to
  approve anything, so only what runs without asking runs. `-c` carries on the last such run.
- **`tiphys log`** lists every tool call and how it came to run or not. `tiphys log verify` checks
  that no entry has been changed or removed.
- **`tiphys spend`** shows what today and this month have cost. A turn stops when the day's total
  reaches the limit, $5.00 unless you set another.
- **`tiphys sessions`** lists sessions. **`tiphys doctor`** checks the installation;
  `tiphys doctor --live` also makes a real tool call on the default connection.

Settings are in `/var/lib/tiphysd/config.toml`, which Tiphys reads and never rewrites.
[`config.example.toml`](config.example.toml) shows what can be set.

## On a workstation

Tiphys also runs by itself, with no service and no second user:

```sh
cargo build --release --locked -p tiphys-cli && ./target/release/tiphys
```

Its state is then in `~/.tiphys`, or wherever `TIPHYS_HOME` points, and the agent acts as you. The
rules about commands are then all that stands between a command and the key file, so this is for
trying Tiphys out, not for leaving it running. `tiphys daemon run` runs the daemon by hand.

## What it will be

- **Scheduled jobs** that run inside a scope you approved and deliver a report to your chat.
- **Memory** kept per chat, and skills, as plain Markdown files.

## Layout

| Crate | Holds |
| --- | --- |
| `crates/tiphys-core` | Config, connections and keys, the provider layer, the agent loop, tools, action policy, action log, sessions |
| `crates/tiphys-tui` | The terminal app |
| `crates/tiphys-cli` | The `tiphys` binary |
| `crates/tiphys-daemon` | The daemon, its socket, the service install and the Telegram adapter; later the job runner |

## License

Apache-2.0. See [`LICENSE`](LICENSE).
