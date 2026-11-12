---
name: wigwag
description: Set up and run wigwag, a Rust Signal daemon, so an AI agent answers in Signal when its name is mentioned, and so two people's agents can talk to each other in a shared chat. Use when someone wants their agent on Signal, wants to install or link wigwag, wants to run wigwag-bridge, wants to connect their agent with a friend's agent, or is debugging either.
---

# wigwag: your agent on Signal

Three pieces, each one replaceable:

```
Signal ⇄ wigwag daemon ⇄ wigwag-bridge ⇄ agent command
         (HTTP + SSE on      (decides when     (claude -p, or any
          127.0.0.1:8080)     to answer)        program: stdin → stdout)
```

- **wigwag** is linked to the person's own Signal account as an extra device, the way
  Signal Desktop is. It exposes signal-cli's HTTP API on localhost.
- **wigwag-bridge** (`bridge/wigwag_bridge.py`, Python 3.10+, stdlib only) watches the
  event stream. It answers when the agent's name is mentioned by the owner or by
  someone on the allow list, and it answers everything in Note to Self.
- **The agent** is any command that reads a prompt on stdin and prints a reply. The
  default is `claude -p`.

Replies go out from the owner's account, prefixed `🤖 <Name>: `. That prefix is the
whole protocol. It tells people and other agents that the message came from an agent.
It keeps a bridge from answering its own replies. And it lets agents count how many
agent turns happened in a row, so a back-and-forth between two agents stops instead of
running forever.

## Setting it up

Do these in order and check each one before moving on. Ask the person for anything
only they can supply: their Signal phone, their agent's name, and their friend's UUID.

1. **Install wigwag.** Get a release binary from
   `https://github.com/hhh42/wigwag/releases`. The builds are
   `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
   `aarch64-apple-darwin`. Each tarball also contains the bridge, the service files
   and this skill.
   ```sh
   v=v0.1.0 t=aarch64-apple-darwin        # or x86_64-/aarch64-unknown-linux-gnu
   curl -fLO https://github.com/hhh42/wigwag/releases/download/$v/wigwag-$v-$t.tar.gz
   tar xzf wigwag-$v-$t.tar.gz && mkdir -p ~/.local/bin
   cp wigwag-$v-$t/wigwag wigwag-$v-$t/wigwag_bridge.py ~/.local/bin/
   ~/.local/bin/wigwag --version
   ```
   The binary isn't signed. If macOS says it "cannot be opened", it was downloaded
   through a browser. Fix it with
   `xattr -d com.apple.quarantine ~/.local/bin/wigwag`. To build from source instead,
   see [setup.md](references/setup.md#building).
2. **Make a passphrase.** It encrypts the account database, which holds the
   account's identity keys.
   ```sh
   mkdir -p ~/.config/wigwag && umask 077
   head -c 32 /dev/urandom | base64 > ~/.config/wigwag/passphrase
   export WIGWAG_PASSPHRASE_FILE=~/.config/wigwag/passphrase
   ```
3. **Link the account.** Run `wigwag link --device-name <agent-name>`. It prints a QR
   code. In Signal on the phone, go to Settings → Linked devices → +, and scan it
   **within about a minute**, because the code expires. If it expires, run the
   command again. Afterwards, `wigwag whoami` prints the number and the ACI UUID.
4. **Run the daemon as a service.** On Linux use `contrib/wigwag.service`, a systemd
   user unit. On macOS use `contrib/wigwag.plist`, a launchd agent. Then check it:
   `curl -s localhost:8080/api/v1/check` should return 200. Details are in
   [setup.md](references/setup.md#services).
5. **Try the bridge in the foreground first**, in dry-run mode:
   ```sh
   python3 bridge/wigwag_bridge.py --name Jarvis --owner David --dry-run -v
   ```
   Have the person send any message to **Note to Self**. The log should show the
   agent running and `[dry run] note-to-self ← 🤖 Jarvis: …`. Then drop `--dry-run`
   and send another one. The reply shows up in Note to Self.
6. **Let a friend in.** Add `--allow <their ACI UUID>`. Get the UUID with
   `wigwag`'s `listContacts` (see [api.md](references/api.md)), or ask the friend to
   run `wigwag whoami`. Once allowed, the friend can mention the agent in a DM or a
   group and get an answer.
7. **Run the bridge as a service** with `contrib/wigwag-bridge.service` or
   `contrib/wigwag-bridge.plist`. Set its working directory to the folder the agent
   should work in, so `claude` picks up that folder's `CLAUDE.md`.

## Two agents talking

Each person runs their own wigwag and bridge, on their own account, and each one
allows the other person's UUID. Then they put both people in a Signal group, or use
their DM. Example: Harold's agent is Chippy and David's agent is Jarvis.

```
Harold:     chippy, ask Jarvis when David lands Friday
🤖 Chippy:  Jarvis, what time does David's flight land Friday?
🤖 Jarvis:  Chippy, 6:40pm at PDX, United 1123.
🤖 Chippy:  Got it. Harold, David lands 6:40pm Friday at PDX (UA 1123).
```

Chippy's last line doesn't say "Jarvis", so Jarvis stays quiet, and the exchange ends.
The rules that make this work, and how to tune them, are in
[agents-talking.md](references/agents-talking.md):

- An agent answers its **name**, in any form: `Jarvis`, `jarvis,` or `@Jarvis`. The
  `🤖 Name:` prefix at the start of a message is not counted as a mention.
- The prompt tells each agent to **say another agent's name only when it needs an
  answer**, and to reply `NO_REPLY` when nothing needs saying.
- After `--max-agent-turns` agent messages in a row (default 4), a bridge stops
  answering other agents until a person says something in that chat.
- `--max-replies-per-hour` (default 60) caps spending, however the chat goes.

## Use cases

- **Scheduling between households.** "Chippy, find a night next week that works for
  David." Chippy asks Jarvis, Jarvis checks David's calendar (if its agent command is
  allowed to), and they settle on a time.
- **Handing off a task.** "Jarvis, send Chippy the trailhead coordinates." The answer
  lands in the shared chat, where both agents and both people can see it.
- **Asking the other side's agent directly.** David writes "chippy, is Harold around
  Saturday?" in their DM. Chippy answers from what Harold's agent knows.
- **A private assistant.** In Note to Self, every message is a prompt. It works like a
  chat with your own agent that you can reach from any Signal device.

Command recipes for each of these (Claude Code with tools, a persona file, other
CLIs) are in [setup.md](references/setup.md#agent-commands).

## Safety, which you must tell the person

- **A linked device sees everything the account receives.** That includes every DM
  and every group. The bridge only *answers* the owner, the allow list and Note to
  Self. But wigwag and the bridge's in-memory history do see other messages, and the
  history is part of the prompt for chats where the agent gets summoned.
- **The HTTP API has no authentication.** Keep it on `127.0.0.1`. Never put it on a
  LAN address or behind a tunnel.
- **Tools mean power.** With no other flags, `claude -p` can't run tools. Only add
  `--allowedTools` for what the agent truly needs, and remember that anyone on the
  allow list can then make it use those tools.
- **Use a release build (0.1.0 or later).** sqlx logs every statement at debug level,
  and that includes the database key. Release builds always keep sqlx at `warn`,
  whatever `RUST_LOG` says. Never share wigwag logs from an older build.

## When something's off

| Symptom | Likely cause |
|---|---|
| `link` QR "expired" or the phone says the code is invalid | Signal's link window is about 60–90 s. Run it again and scan straight away. |
| `check` returns 503 | The device was unlinked from the phone. Run `wigwag link` again. |
| Bridge logs nothing when a friend writes | Their UUID isn't in `--allow`, or they didn't say the name. Run with `-v`. |
| Sending to `+number` fails | wigwag only knows numbers that are in contacts. Use the ACI UUID. |
| Agents stop answering each other | That's the turn limit. Someone says anything in the chat and it resets. |
| Replies appear twice | Two bridges are running for the same account. Run one per account. |
