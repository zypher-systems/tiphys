# Roadmap

Living plan for Tiphys. Milestones are work, not version numbers: the version stays 0.1.0 until a
release is decided.

## What 1.0 means

**A draft, not yet agreed.** It was written when the first version worked, for the owner to change
and confirm. Tiphys stays below 1.0 until every item holds and the release is decided.

- [ ] A fresh Ubuntu server goes from a release to a working conversation in chat by following the
      README, with no step the README does not cover.
- [ ] It runs unattended for 30 days: no restart, the state directory stays a sensible size, and it
      comes back by itself after a reboot, a network loss, a provider outage and its own upgrade.
- [ ] Only the daemon writes state. A `kill -9` at any moment loses at most the turn in flight, and
      every file still reads back.
- [ ] Every action is in the action log, and `tiphys log verify` passes at the end of the 30 days.
- [ ] No message from an account off the allowlist reaches the model.
- [ ] The rules are proven on real use: a kept list of real commands with the class each should
      get, and no surface, setting or scheduled job gets past a refusal.
- [ ] The key store cannot be read by a command the agent runs, by a boundary the operating system
      enforces and not only by reading the command line.
- [ ] An approval never hangs a turn: it is answered, denied or timed out, and the model is told
      which.
- [ ] Scheduled jobs collapse missed runs into one, stay inside their scope and budget, and report
      when they are blocked.
- [ ] Budgets hold across chat, jobs and one-shot runs, and a model with no known price never runs
      unattended under a cap.
- [ ] Each wire format passes a live tool-call round trip on the release, parallel calls included.
- [ ] A conversation that never resets stays usable after many compactions.
- [ ] Memory is scoped: what one chat said is never in another chat's prompt.
- [ ] No key appears in a prompt, a tool result, the action log, a diagnostic log or a chat message.
- [ ] Upgrades keep data: every file format carries a version and is tested from every release.
- [ ] The README, the guide and `config.example.toml` match the release.

Out of scope for 1.0: a web console, an MCP client, dispatch to other agents, chat platforms
beyond the first, more than one owner.

## Now

**M0: a first conversation.** Built, and run by the owner against a real provider.

- [x] Workspace, config, key store, crash-safe JSONL, CI.
- [x] Provider layer: Chat Completions with streaming and tool calls, a replay provider, spend.
- [x] Agent loop: sessions, events, the tool registry, read-only tools, `tiphys -p`.
- [x] Terminal app: first-run setup, connections with masked key entry, a model picker, a real
      tool-call check of the chosen model before it is saved, chat.
- [x] File actions: the path rules, approvals with a card in the app, `write_file`, `edit_file`,
      the action log and `tiphys log`.
- [x] Commands: the `shell` tool with its Ubuntu rules, and `tiphys doctor`.
- [x] A live round trip with a real provider. On 2026-10-09 the owner set up an OpenRouter
      connection in the app and asked for an overview of the machine: 12 tool calls over two
      turns, two of them made in one reply, every call priced, three approvals asked and given,
      and the action log verified afterwards.
- [ ] The rest of the "done when" below, on the real connection: a file written on request, and a
      `sudo` request that shows a card.
- [ ] Captures of a real provider's streams in `crates/tiphys-core/fixtures/chat/`, in place of the
      ones written by hand. Nothing records a stream yet.

Done when: you run `tiphys`, add a connection and its key in the app, ask "how full is the root
disk? write the answer to ~/disk.txt", and the file appears; a `sudo` request shows an approval
card; `tiphys log verify` passes.

Known gaps in what is built:

- The shell rules have met their own tests and one real session. In that session 3 of 10 commands
  asked: two `for` loops and a program not in the table. Real use will find commands that ask and
  should not, and the reverse.
- A command the agent runs is the same user as the agent, so the key store is protected from it by
  the rules alone. M1 gives it a boundary the operating system enforces.
- There is no spending limit yet.
- The conversation is plain text: Markdown is not drawn, and a long diff in an approval card cannot
  be scrolled.

**M1: the daemon.** Under way. The order is set by what can be done before there is a server to
install on.

- [x] A host that serves several clients: attach and leave, the conversation replayed to a client
      that comes late, answers to a client's own questions sent to it alone, the last session
      picked up after a restart, a turn cut short by a restart closed.
- [x] The daemon: `tiphys daemon run` behind a Unix socket, one host per audience, clients checked
      by who they are, a clean stop on SIGTERM. The app and `tiphys -p` are its clients when it is
      running, and run by themselves when it is not.
- [x] A worker: the agent's actions, file tools and commands alike, are planned and run in a
      `tiphys worker` process that the daemon starts with a configured command. Run by hand that
      is the same user; installed, it is a second user who cannot read the keys or the state.
- [ ] `tiphys daemon install`: the users, the unit, the directories, and the owner who may connect.
      A daily spending limit, on by default. A key read from the environment does not stay in it.
- [x] `install.sh` for Ubuntu, and a release workflow: a tag on main builds static binaries for
      x86_64 and arm64 and drafts the release. CI builds the same archives and runs the installer on
      them on every push. The installer hands over to `tiphys daemon install` once that exists.
- [ ] The first release, when the owner asks for it, and the install on a real server.

Done when: it is installed on an Ubuntu server from a release, you open the app as yourself, drop
SSH in the middle of a turn, reconnect, and the turn replays and finishes. That last part already
holds on a workstation: a terminal killed mid-turn and reopened shows the turn and its end.

## Next

**M2: Telegram.** The adapter, an allowlist, approvals in chat, compaction. The bot token is entered
in the app.
Done when: a stranger gets nothing, you get a streamed answer, a system change shows buttons, an
unanswered request is denied after 300 seconds, and `/stop` and `/new` work.

**M3: memory and jobs.** Bounded memory scoped per chat. Scheduled jobs with delivery to your chat.
Done when: "every day at 07:00 check disk and failed units, tell me only if something is wrong"
becomes a job you approve, a missed run catches up once, and a preference survives `/new`.

**M4: web.** `web_fetch` and `web_search`.
Done when: a question that needs a search answers with links, and fetches of metadata and LAN
addresses are refused.

**M5: identity and skills.** An identity file, skills, memory search, group chats.
Done when: a saved skill runs by name, and a group chat never sees the private chat's memory.

## Later

- More wire formats: Anthropic Messages, the Responses API.
- An MCP client, over stdio and Streamable HTTP.
- Dispatch to external agents: one tool over `[agents.<name>] command = [...]`, for any agent CLI.
- More chat platforms.
- A `.deb` package and a signed self-update.
- A web console.
- Sandboxing for tool calls.

## Done

Nothing yet.

## Blocked

Nothing.
