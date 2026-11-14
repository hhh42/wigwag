# Security

wigwag holds Signal identity keys and can read and send messages as its account.
Please report vulnerabilities privately through GitHub's security advisory form
on this repository rather than in a public issue.

Bugs in the Signal protocol or in presage belong upstream:
[libsignal](https://github.com/signalapp/libsignal/security) and
[presage](https://github.com/whisperfish/presage).

## Threat model

### What's worth stealing

| Asset | Where | Protection |
|---|---|---|
| Identity keys and session state | `wigwag.db` in the data directory | SQLCipher with `--passphrase-file`; directory `0700`; files `0600` (umask `077`) |
| Database passphrase | `--passphrase-file` | must be `0600` and yours, or wigwag refuses it; never passed to the agent |
| API token | `--token-file` | must be `0600` and yours, at least 16 characters, compared in constant time |
| Messages and attachments | memory, the event backlog, `attachments/` | messages purged from the database hourly (unless `--keep-messages`); attachments deleted after `--attachment-ttl-days` |
| The ability to send as you | the API, the agent | below |

### Who might attack, and what stops them

**A web page in your browser.** Browsers can send requests to `127.0.0.1`, and a
`text/plain` POST needs no CORS preflight, so without a defence any page could call
`send`. wigwag refuses every request with an `Origin` or `Sec-Fetch-*` header, which
browsers attach and signal-cli clients don't. DNS rebinding (a page pointing its own
hostname at 127.0.0.1) is stopped by accepting only `localhost`, `127.0.0.1` or
`[::1]` as `Host` on loopback and on the Unix socket.

**Another user on the same machine.** With `--socket`, the socket is `0600`, and the
service files put it in a `0700` directory: they can't connect. On TCP, they can, unless you set
`--token-file`. They can't read the data directory, the token or the passphrase.

**Another program running as you.** This is the hard case: anything running as your
user can, in the end, read your files. wigwag narrows it. The process is
non-dumpable on Linux (no ptrace, no `/proc/<pid>/mem`), never writes core dumps,
and with `--token-file` the API needs a secret that a confused or sandboxed program
won't have. For a real boundary, run wigwag as its own user.

**Someone on your network.** wigwag binds `127.0.0.1` by default and warns loudly if
you bind anything else without a token. There is no TLS: for remote access, use an
SSH tunnel (`ssh -N -L 8080:127.0.0.1:8080 host`) rather than exposing the port.

**A client of the API (a bot, or someone who got in).** Path attachments are fenced:
wigwag won't read anything in its data directory except downloaded attachments,
anything that isn't a regular file (`/dev/zero`, FIFOs), or anything over
`--max-attachment-mib`; `--send-dir` narrows paths to one folder. Attachment ids
can't leave the attachments directory. At most 16 event streams may be open.
Responses are `Cache-Control: no-store` and `nosniff`.

**People in your chats (prompt injection against the agent).** The agent answers
only the owner, the allow list and Note to Self, but other people's messages in the
same chat are in its prompt. Every line not from the owner is labelled
`Name (not <owner>)`, because profile names are self-chosen, and the prompt tells
the agent that only the owner gives instructions. That's guidance, not a guarantee.
The guarantee has to come from what the agent can do:

- The agent command runs as you. Whatever it can do, a successful injection can do,
  including talking to wigwag. Give it no tools and no MCP connectors
  (`claude -p --tools "" --strict-mcp-config`) unless you need them, and run it in an
  empty folder. Recent `claude` versions also take `--restricted`, which ignores
  your settings files (and so their hooks) as well. For a hard boundary, run it as
  another user or in a container.
- It never sees wigwag's `WIGWAG_*` environment, runs in its own process group that
  is killed on timeout, has its output capped (256 KiB read, 4000 characters sent),
  and can only reply into the chat it was summoned in.
- `--max-agent-turns` stops agent-to-agent loops; `--max-replies-per-hour` caps
  spending.

**A malicious Signal contact.** wigwag parses what presage decrypts; it never
executes message content. Inbound attachments over the size limit aren't downloaded,
and attachment file names never become paths (ids are hashes).

**The supply chain.** `Cargo.lock` is committed and every build uses `--locked`;
presage and its SQLCipher fork are pinned to exact commits. CI pins every GitHub
Action to a commit SHA, runs with read-only permissions, and runs `cargo audit` on
every change and weekly. Dependabot proposes updates. Release tarballs ship with
SHA-256 sums and a signed build-provenance attestation:

```sh
gh attestation verify wigwag-<version>-<target>.tar.gz --repo hhh42/wigwag
```

### What wigwag doesn't protect against

- **Root, or malware already running as you**, with time to read your files.
- **Safety-number changes.** New identities are trusted automatically, as with
  signal-cli's `--trust-new-identities always`: a bot can't verify them.
- **presage itself.** It is not an official Signal client; its bugs are wigwag's.
- **Logs from builds before 0.1.0**, which could print the database key at debug
  level. Current builds pin sqlx to `warn` whatever `RUST_LOG` says. wigwag doesn't
  log message text; the exceptions are the agent's reply under `--agent-dry-run` (that's
  its point) and the last 500 characters of a failing agent command's stderr.

## Hardening checklist

```sh
# Private secrets
mkdir -p ~/.config/wigwag && chmod 700 ~/.config/wigwag
(umask 077; head -c 32 /dev/urandom | base64 > ~/.config/wigwag/passphrase)
(umask 077; openssl rand -hex 32 > ~/.config/wigwag/token)

# Socket, not a port; token anyway; attachments only from one folder
wigwag daemon --socket "$XDG_RUNTIME_DIR/wigwag/api.sock" \
  --passphrase-file ~/.config/wigwag/passphrase \
  --token-file ~/.config/wigwag/token --send-dir ~/wigwag-outbox

# Agent only: no API at all, and an agent with no tools
wigwag daemon --no-http --agent Chippy --owner Harold \
  --agent-command 'claude -p --tools "" --strict-mcp-config'
```

The systemd unit in `contrib/` adds a sandbox on top (read-only system and home
apart from the data directory, no new privileges, a system-call filter, W^X memory).
