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

**M2: Telegram.** Built, and waiting for its first run against the real Telegram. M0 and M1 are
done and released; the last release is 0.1.1.

- [x] Compaction: before a conversation is more than the model can hold, its oldest part is left
      out of what the model is sent: old tool results first, then whole turns. Nothing is deleted.
- [x] Telegram in the daemon: long polling, one conversation per chat, an allowlist, approvals as
      buttons, `/new` and `/stop`.
- [x] Setup in the app: the bot token in a masked field, and pairing by sending the bot a code.
- [ ] Checked on a real server with a real bot.

Done when: a stranger gets nothing, you get an answer, a request that has to ask shows buttons, an
unanswered one is denied after 300 seconds, and `/stop` and `/new` work.

Known gaps in Telegram:

- Text only, and plain: a photo or a file gets "Tiphys reads text only for now", and a model that
  writes Markdown anyway shows its asterisks.
- A user is taken off the allowlist by editing `settings.toml`; the app only adds.
- Everything was built against a stand-in for the Bot API. Rate limits and odd updates from the
  real one have not been met.

Found on the first installed server, and not yet dealt with:

- The app draws Markdown as it arrives: a fenced block shows its backticks.
- Captures of a real provider's streams are still owed for `crates/tiphys-core/fixtures/chat/`;
  nothing records a stream.
- The shell rules have met their own tests and three real sessions. `for` loops and unknown
  programs ask, which is the commonest interruption so far.

## Next

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

**M1: the daemon** (0.1.0). A daemon behind a Unix socket with the app and `tiphys -p` as its
clients; a worker that acts as a second user who cannot read the keys or the state;
`tiphys daemon install`; `install.sh` and a release workflow; the owner's reports through the
daemon; a daily spending limit.
Checked on 2026-10-09 on an Ubuntu 24.04 server installed from the release with `install.sh`, with
a real provider: `doctor` passed with the agent acting as `tiphys`; a turn ran a command and wrote
a file as that user; a `sudo` request showed an approval card, was approved, and failed as it
should for a user with no sudo; the SSH connection was killed in the middle of a turn, and on
reconnecting the turn was shown and then finished; the action log verified.

**M0: a first conversation** (0.1.0). The terminal app with setup and masked key entry, a model
checked by a real tool call before it is saved, the agent loop and sessions, file and shell tools,
the rules and approvals, the action log, spend.
Checked on 2026-10-09 by the owner against a real provider: 12 tool calls over two turns, every
call priced, three approvals asked and given, the action log verified.

## Blocked

Nothing.
