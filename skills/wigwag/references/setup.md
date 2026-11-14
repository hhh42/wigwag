# Setup details

## Building

You need recent stable Rust and `protoc`. Install it with `brew install protobuf` or
`apt install protobuf-compiler`.

```sh
cargo install --locked --git https://github.com/hhh42/wigwag
```

To build on a Linux box that has no Rust, use Docker:

```sh
docker run --rm -v "$PWD":/src -w /src rust:1-bookworm bash -c \
  'apt-get update -qq && apt-get install -y -qq protobuf-compiler && cargo build --release --locked'
# then: target/release/wigwag
```

## Services

The data directory defaults to `~/.local/share/wigwag` on Linux and
`~/Library/Application Support/wigwag` on macOS. The daemon and `link` must use the
same data directory and the same passphrase.

### Linux (systemd user units)

```sh
cp contrib/wigwag.service ~/.config/systemd/user/
# edit wigwag.service: uncomment the agent lines (name, owner, allow, WorkingDirectory)
systemctl --user daemon-reload
systemctl --user enable --now wigwag
loginctl enable-linger "$USER"     # keep it running when you're logged out
journalctl --user -u wigwag -f
```

### macOS (launchd agents)

launchd doesn't expand `~`, so write your home path into the files:

```sh
sed "s|/Users/YOU|$HOME|g" contrib/wigwag.plist > ~/Library/LaunchAgents/org.wigwag.wigwag.plist
# edit it: uncomment the agent keys (name, owner, allow, WorkingDirectory)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/org.wigwag.wigwag.plist
tail -f ~/Library/Logs/wigwag.log
```

To restart it: `launchctl kickstart -k gui/$(id -u)/org.wigwag.wigwag`.

## Agent commands

The agent runs `--agent-command` once per reply. It writes the prompt to stdin, reads
the reply from stdout, and allows up to `--agent-timeout` seconds. The prompt already contains
the recent chat and the house rules. The command also gets these environment
variables: `WIGWAG_AGENT_NAME`, `WIGWAG_CHAT` (for example `group:<id>`,
`dm:<uuid>` or `note-to-self`), `WIGWAG_SENDER`, and `WIGWAG_FROM_AGENT` (the other
agent's name, or empty).

**Claude Code, chat only.** No tools and no MCP connectors, so it can only talk.

```sh
--agent-command 'claude -p --tools "" --strict-mcp-config'
```

**Claude Code with a few tools.** Run wigwag from the project folder, so that
folder's `CLAUDE.md`, MCP servers and memory all apply.

```sh
--agent-command 'claude -p --allowedTools "Read,Grep,WebSearch,mcp__calendar__*"'
```

Each tool you add can be triggered by everyone on the allow list. Choose the way you
would for a shared account.

**Pick the model.**

```sh
--agent-command 'claude -p --model haiku'   # cheaper, quicker replies
```

**A persona.** This goes into every prompt.

```sh
cat > persona.md <<'EOF'
You're Jarvis, David's agent. Dry humour, short answers. David's in Brooklyn (ET).
Never share David's address or phone number.
EOF
--agent-instructions persona.md
```

**Any other CLI.** Anything that reads stdin and prints works:

```sh
--agent-command 'ollama run llama3.2'
--agent-command 'llm -m gpt-4o-mini'
--agent-command './my-agent.sh'          # your own script
```

A script can branch on the environment, for example to stay quiet with other agents
in one group:

```sh
#!/bin/sh
[ -n "$WIGWAG_FROM_AGENT" ] && [ "$WIGWAG_CHAT" = "group:abc=" ] && { echo NO_REPLY; exit; }
exec claude -p
```

## Registering a number of its own (instead of linking)

`wigwag register +1555… --captcha 'signalcaptcha://…'` gives the bot its own
identity. Get the captcha at <https://signalcaptchas.org/registration/generate.html>.
The number has to receive Signal's SMS. Google Voice numbers usually don't, and
Signal rate-limits retries hard. For agents that act *for* a person, linking to that
person's own account is simpler, and it's what the two-agent setup assumes.
