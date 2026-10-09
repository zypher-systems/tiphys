# Roadmap

Living plan for Tiphys. Milestones are work, not version numbers: the version stays 0.1.0 until a
release is decided.

## What 1.0 means

Not written yet. The list is drafted once a first working version exists (the end of M0) and agreed
with the owner. Until every item on it holds and the release is decided, Tiphys stays below 1.0.

## Now

**M0: a first conversation.** The terminal app with the agent running in the same process.

- [x] Workspace, config, key store, crash-safe JSONL, CI.
- [x] Provider layer: Chat Completions with streaming and tool calls, a replay provider, spend.
      The wire fixtures are written by hand; captures from a real provider are still owed, and so
      is the live round trip, which waits for the app.
- [x] Agent loop: sessions, events, the tool registry, read-only tools, `tiphys -p`.
- [x] Terminal app: first-run setup, connections with masked key entry, a model picker, a real
      tool-call check of the chosen model before it is saved, chat.
- [x] File actions: the path rules, approvals with a card in the app, `write_file`, `edit_file`,
      the action log and `tiphys log`.
- [ ] Commands: the `shell` tool with its Ubuntu rules, and `doctor`.

Done when: you run `tiphys`, add a connection and its key in the app, ask "how full is the root
disk? write the answer to ~/disk.txt", and the file appears; a `sudo` request shows an approval
card; `tiphys log verify` passes.

## Next

**M1: the daemon.** The daemon and its socket, the app as a client, a system unit,
`tiphys daemon install`, an installer for Ubuntu.
Done when: it is installed on an Ubuntu VM, you open the app as yourself, drop SSH in the middle of
a turn, reconnect, and the turn replays and finishes.

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
