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
| `--passphrase-file` / `WIGWAG_PASSPHRASE_FILE`, or `WIGWAG_PASSPHRASE` | none | encrypts the database with SQLCipher |
| `daemon --http` | `127.0.0.1:8080` | |
| `daemon --keep-messages` | off | presage stores every message; by default wigwag purges them hourly |
| `daemon --attachment-ttl-days` | 7 | downloaded attachments are deleted after this |
| `daemon --max-attachment-mib` | 100 | |
| `daemon --backlog` | 1000 | envelopes held while no client is connected |
| `RUST_LOG` | `info` | e.g. `RUST_LOG=wigwag=debug` |

There is a systemd user unit in [`contrib/wigwag.service`](contrib/wigwag.service).

## Security

- **The API has no authentication**, the same as signal-cli's. Anything that can
  reach the port can read your messages and send as you. wigwag binds to
  `127.0.0.1` by default and warns you if you bind it anywhere else.
- **Encrypt the database.** It holds your account's identity keys. Without a
  passphrase it is plain SQLite.
- **New safety numbers are trusted automatically**, like signal-cli's
  `--trust-new-identities always`, because a bot can't verify them. If you need
  identity verification, wigwag is the wrong tool.
- **A linked device can read everything** the account receives. Linking wigwag to
  your personal account gives the bot your messages. Restrict who it answers on
  the bot side (for example with Hermes's `SIGNAL_ALLOWED_USERS`).

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
