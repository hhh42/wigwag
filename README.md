# wigwag

A small Signal daemon in Rust that speaks [signal-cli]'s HTTP JSON-RPC API.

Bots and agents that already talk to `signal-cli daemon --http` can point at wigwag
instead and keep working. wigwag is a single binary of about 20 MB. It needs no
JVM, has no runtime dependencies, and starts in well under a second.

It is built on [presage] and [libsignal-service-rs], which in turn sit on Signal's
own [libsignal]. The Signal protocol comes from those libraries. wigwag is the daemon
and the translation layer around them.

> **wigwag** is the flag code the US Army used to send messages across valleys in the
> 1860s: one person, one flag, no machinery. [Why it's called that](#why-wigwag).


**Status: early.** wigwag covers what chat bots use. Other clients using the same
API will hit methods it doesn't implement yet. It is not affiliated with Signal
Messenger or with signal-cli.

## What it speaks

| Endpoint | |
|---|---|
| `GET /api/v1/check` | `200` while connected; `503` once the device is unlinked |
| `POST /api/v1/rpc` | JSON-RPC 2.0, single calls or batches |
| `GET /api/v1/events` | Server-sent events, one `receive` event per envelope |

The SSE events use signal-cli's envelope shape (`{"envelope": {...}, "account": "+1..."}`),
including `dataMessage`, `syncMessage.sentMessage` (Note to Self), `editMessage`,
`typingMessage`, `receiptMessage` and `storyMessage`. When no client is connected,
envelopes are held, up to `--backlog` of them, for the next client that connects.

| Method | Params |
|---|---|
| `send` | `recipient` / `groupId` / `noteToSelf`, `message`, `textStyle(s)`, `mention`, `attachment(s)` (paths or `data:` URIs), `quoteTimestamp` + `quoteAuthor` (+ `quoteMessage`), `viewOnce` |
| `sendTyping` | target, `stop` |
| `sendReaction` | target, `emoji`, `targetAuthor`, `targetTimestamp`, `remove` |
| `getAttachment` | `id` → `{"data": "<base64>"}` |
| `listContacts`, `getContact`, `listGroups`, `listAccounts`, `version` | |

Recipients can be given as `+E164`, as an ACI UUID, as `PNI:<uuid>` or as `u:<username>`.
Error codes follow signal-cli. Rate limits come back as code `-5` with
`retryAfterSeconds`, so existing backoff logic keeps working.

## Install

Grab a binary from the releases, or build one:

```sh
# needs a recent stable Rust and protoc (apt install protobuf-compiler / brew install protobuf)
cargo install --locked --git https://github.com/hhh42/wigwag
```

## Set up an account

wigwag can use an account in either of two ways.

**Link to your account** as an extra device, the way Signal Desktop is linked. The
bot sees your conversations, and you talk to it in *Note to Self*.

```sh
wigwag link --device-name my-bot
# scan the QR code: Signal → Settings → Linked devices → +
```

**Register a number of its own**, so people message the bot like any other contact:

```sh
# get a captcha token at https://signalcaptchas.org/registration/generate.html
wigwag register +15551234567 --captcha 'signalcaptcha://…'
wigwag profile --name "My Bot"
```

Then run it:

```sh
wigwag daemon --http 127.0.0.1:8080
wigwag whoami
wigwag send +15557654321 -m "hello from wigwag"
```

## Configuration

| Flag / env | Default | |
|---|---|---|
| `--data-dir` / `WIGWAG_DATA_DIR` | `~/.local/share/wigwag` (Linux) | account database and attachments |
| `--passphrase-file` / `WIGWAG_PASSPHRASE_FILE`, or `WIGWAG_PASSPHRASE` | none | encrypts the database with SQLCipher; the file must be `0600` |
| `daemon --http` | `127.0.0.1:8080` (unless `--socket`) | TCP address |
| `daemon --socket` / `WIGWAG_SOCKET` | none | Unix socket, mode `0600`; replaces the TCP default |
| `daemon --no-http` | off | serve no API (agent only) |
| `daemon --token-file` / `WIGWAG_TOKEN_FILE` | none | require `Authorization: Bearer`; file must be `0600` |
| `daemon --send-dir` / `WIGWAG_SEND_DIR` | none | path attachments only from here |
| `daemon --keep-messages` | off | presage stores every message; by default wigwag purges them hourly |
| `daemon --attachment-ttl-days` | 7 | downloaded attachments are deleted after this |
| `daemon --max-attachment-mib` | 100 | |
| `daemon --backlog` | 1000 | envelopes held while no client is connected |
| `RUST_LOG` | `info` | e.g. `RUST_LOG=wigwag=debug` |

There is a systemd user unit in [`contrib/wigwag.service`](contrib/wigwag.service).

## Security

wigwag holds your Signal identity keys and can read and send as your account, so it
is built to expose as little as possible. [SECURITY.md](SECURITY.md) has the full
threat model. The short version:

- **Prefer the Unix socket.** `wigwag daemon --socket <path>` serves the API on a
  socket with mode `0600`, so only your user can connect, with no port at all. The
  service files in `contrib/` use it. With `--no-http` (agent only) nothing is served.
- **TCP has no authentication by default**, the same as signal-cli's, because that's
  what signal-cli clients expect. Anything that can reach the port can read your
  messages and send as you. wigwag binds `127.0.0.1` and warns about anything else.
  Add a token to keep other programs and users out:

  ```sh
  (umask 077; openssl rand -hex 32 > ~/.config/wigwag/token)
  wigwag daemon --token-file ~/.config/wigwag/token
  ```

  Clients then send `Authorization: Bearer <token>`. To reach it from another
  machine, keep it on loopback and tunnel: `ssh -N -L 8080:127.0.0.1:8080 host`.
- **Web pages are refused.** Any request carrying `Origin` or `Sec-Fetch-*` (which
  browsers add) gets `403`, and on loopback or the socket so does any `Host` other
  than `localhost`/`127.0.0.1`/`[::1]` (DNS rebinding). Without this, any page you
  opened could POST a `send` to `127.0.0.1:8080`.
- **Attachments by path are fenced.** wigwag refuses anything in its own data
  directory (the keys), anything that isn't a regular file, and anything over the
  size limit. `--send-dir <dir>` narrows it to one folder. `data:` URIs always work.
- **Secret files must be private.** `--passphrase-file` and `--token-file` are
  refused if group or others can read them, or if you don't own them, as ssh does.
- **The process is locked down.** umask `077` for everything it creates, no core
  dumps, and on Linux it is non-dumpable, so other processes of your own user can't
  read its memory or ptrace it.
- **Encrypt the database.** It holds your account's identity keys. Without a
  passphrase it is plain SQLite.
- **New safety numbers are trusted automatically**, like signal-cli's
  `--trust-new-identities always`, because a bot can't verify them. If you need
  identity verification, wigwag is the wrong tool.
- **A linked device can read everything** the account receives. Linking wigwag to
  your personal account gives the bot your messages. Restrict who it answers on
  the bot side (for example with Hermes's `SIGNAL_ALLOWED_USERS`).

### The agent

Treat everything in a chat as hostile input to the agent: prompt injection.

- It answers only the owner, `--allow` and Note to Self, but other people's messages
  in the same chat are in its prompt. Each of those lines is labelled
  `Name (not <owner>)`, since anyone can set their profile name to yours.
- **It runs as you.** Whatever the agent command can do, a successful prompt
  injection can do, including connecting to wigwag itself. Give it no tools and no
  connectors unless you need them, and run it in an empty folder:
  `--agent-command 'claude -p --tools "" --strict-mcp-config'`. For real isolation,
  run it as another user (`sudo -u wigwag-agent claude -p …`) or in a container.
- It gets none of wigwag's `WIGWAG_*` environment (passphrase included), runs in
  its own process group that is killed on timeout, and its output is capped
  (256 KiB read, 4000 characters sent). Replies go only to the chat it was asked in.
- `--max-agent-turns` and `--max-replies-per-hour` bound how much it can be made to
  say, and to spend.

## Known limits

- **Phone numbers.** Signal only tells you someone's number if they share it.
  Incoming messages from an unknown number are keyed by UUID, and you can only
  send to a `+E164` number that is in your contacts. Use UUIDs. Contact discovery
  (CDSI) isn't wired up yet.
- No calls, stories, polls, payments, profile editing beyond the name, or group
  administration.
- Only one account per daemon. The `account` param is checked, not used to pick
  an account.
- presage is not an official Signal client. When Signal changes its servers,
  things can break until presage catches up. Keep wigwag up to date.

## Your agent on Signal

`wigwag daemon --agent <Name>` turns wigwag into an agent that answers to its name.
It's built in: no second process, no Python. When the account owner, or someone
you've allowed, mentions the agent's name in any chat, it runs your agent command
and posts the reply into that chat. It answers everything you write in Note to Self.

```sh
wigwag daemon --no-http --agent Jarvis --owner David \
  --allow <a friend's ACI UUID> --agent-command 'claude -p --tools "" --strict-mcp-config'
```

Try it first with `--agent-dry-run`, which runs the agent but sends nothing. See
`wigwag daemon --help` for the rest (`--agent-alias`, `--agent-instructions`,
`--agent-timeout`, `--max-agent-turns`, `--max-replies-per-hour`, …).

Replies start with `🤖 Jarvis: `, because they go out from your own account. Two
people who each run one can put their agents in the same group, and the agents
talk to each other:

```
Harold:     chippy, ask Jarvis when David lands Friday
🤖 Chippy:  Jarvis, what time does David's flight land Friday?
🤖 Jarvis:  Chippy, 6:40pm at PDX, United 1123.
🤖 Chippy:  Got it. Harold, David lands 6:40pm Friday at PDX (UA 1123).
```

Each agent is told to say the other's name only when it needs an answer. Each
one also stops answering other agents after `--max-agent-turns` (default 4)
agent messages in a row, until a person says something. So two agents can't loop
forever. The agent command can be anything that reads a prompt on stdin and prints
a reply: `claude -p`, `ollama run …`, `llm`, or your own script.

### Claude Code skill

The repo ships a skill that walks Claude through the whole setup: install, link,
services, the agent, and pairing with a friend's agent. It comes with examples and
troubleshooting. It's in [`skills/wigwag`](skills/wigwag/SKILL.md).

```
/plugin install wigwag --marketplace hhh42/wigwag
```

Claude Code picks the skill up automatically inside a clone of this repo.

## Using it with Hermes Agent

The [Hermes] Signal platform plugin works with wigwag unchanged:

```sh
SIGNAL_HTTP_URL=http://127.0.0.1:8080
SIGNAL_ACCOUNT=+15551234567          # the number wigwag is registered or linked as
SIGNAL_ALLOWED_USERS=<your uuid or number>
```

## Why "wigwag"

Wigwag is the flag code of the US Army's first signal service. Albert J. Myer, an
Army surgeon, built it in the 1850s. His medical thesis had been a sign language for
deaf people, and he turned that idea into a code you could read from miles off. The
Army made him its first Signal Officer in 1860, and that was the start of the Signal
Corps.

The code needs one flag by day, or a torch at night. Swing it to the left for `1` and
to the right for `2`. Each letter is a short run of those swings, and dipping the flag
forward ends a word. The back-and-forth swinging gave the code its name. With it, one
soldier on a hilltop could talk to another miles away, with no wire and nothing to
carry but a pole.

Its first use in combat came at First Bull Run in July 1861, and it came from the
other side. Edward Porter Alexander had helped Myer test the code before the war, and
by then he was a Confederate officer. From a hilltop he saw sunlight glint off Union
bayonets that were moving around the Confederate flank. He wigwagged a warning to the
brigade on that flank: *"Look out for your left; you are turned."*

The name fits this project for three reasons:

- **It is small.** signal-cli is a good program, but running it means running a Java
  VM. wigwag is one Rust binary, the way the code was one flag on a pole.
- **It goes back and forth.** A wigwag station both sends and reads, and wigwag
  carries messages both ways between Signal and your bot.
- **It isn't "Signal".** Signal asks third-party projects not to use its name. Wigwag
  is the Army's own original signal code, so the name keeps that link without
  borrowing a trademark.

## License

AGPL-3.0-only, the same license as presage and libsignal, which wigwag links against.

[signal-cli]: https://github.com/AsamK/signal-cli
[presage]: https://github.com/whisperfish/presage
[libsignal-service-rs]: https://github.com/whisperfish/libsignal-service-rs
[libsignal]: https://github.com/signalapp/libsignal
[Hermes]: https://github.com/NousResearch/hermes-agent
