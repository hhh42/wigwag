# Changelog

## Unreleased

- **The agent bridge is now Rust, inside the daemon:** `wigwag daemon --agent <Name>`.
  `bridge/wigwag_bridge.py` and its service units are gone. Flags map as `--name` →
  `--agent`, `--alias` → `--agent-alias`, `--command` → `--agent-command`,
  `--dry-run` → `--agent-dry-run`, `--instructions`/`--timeout`/`--history` →
  `--agent-…`; the rest keep their names. `--no-http` runs the agent with no port open.
- **Security:** the HTTP API refuses requests with an `Origin` header (web pages could
  POST `send` to localhost) and, on loopback, any non-loopback `Host` (DNS rebinding).
- `--token-file`: require `Authorization: Bearer <token>`.
- `--send-dir`: path attachments must resolve inside it.
- The agent's prompt labels every line not from the owner, so a stranger can't pose as
  the owner by profile name, and says only the owner gives instructions.
- `--socket <path>`: serve the API on a Unix socket (mode `0600`) instead of TCP. The
  systemd and launchd files now use it.
- Browser requests are also recognised by `Sec-Fetch-*` headers. Responses carry
  `Cache-Control: no-store` and `nosniff`. At most 16 event streams at once.
- Path attachments: never from wigwag's data directory (the keys) apart from its
  attachments, never a device or FIFO, and sizes are checked before reading.
- `--passphrase-file` and `--token-file` must be `0600` and owned by you, or wigwag
  refuses them. Using `WIGWAG_PASSPHRASE` from the environment now warns.
- Process hardening: umask `077`, no core dumps, and non-dumpable on Linux (no ptrace
  or `/proc/<pid>/mem` from other processes of the same user).
- Agent: no `WIGWAG_*` variables (passphrase included) reach the agent command; it runs
  in its own process group, killed on timeout; output is capped at 256 KiB read and
  4000 characters sent.
- systemd unit: `UMask=0077`, `ProtectHome=read-only`, a system-call filter,
  `MemoryDenyWriteExecute`, namespace and address-family restrictions.
- CI: every action pinned to a commit SHA, read-only permissions, no persisted
  credentials, `cargo audit` on every change and weekly, Dependabot. Releases carry a
  signed build-provenance attestation (`gh attestation verify`).
- `SECURITY.md` now has a threat model and a hardening checklist.

## 0.1.0 — 2026-11-12

First version. Named **wigwag** after the US Army's 1860 flag code; see "Why wigwag" in the README.

- Link as a secondary device, or register a number of its own.
- signal-cli-compatible HTTP API: `/api/v1/check`, `/api/v1/rpc` (JSON-RPC 2.0, batches),
  `/api/v1/events` (SSE).
- RPC methods: `send`, `sendTyping`, `sendReaction`, `getAttachment`, `listContacts`,
  `getContact`, `listGroups`, `listAccounts`, `version`.
- Envelopes for data, Note to Self / sync-sent, edit, typing, receipt and story messages.
- Inbound attachments downloaded on arrival and pruned after a TTL; stored messages purged hourly.
- Events held for the next subscriber while none is connected.
- `wigwag-bridge` (`bridge/wigwag_bridge.py`): an agent that answers to its name in Signal
  by running any stdin→stdout command (default `claude -p`). It answers every Note to Self
  message, has an allow list, and prefixes replies with `🤖 Name: `. Agent-to-agent turn
  limits, `NO_REPLY` and an hourly budget keep two agents from looping.
- A Claude Code skill and plugin (`skills/wigwag`, `.claude-plugin/`) for setting up and
  pairing agents.
- systemd and launchd units for the daemon and the bridge (`contrib/`).
- sqlx is held at `warn` whatever `RUST_LOG` says. Its debug output includes the
  database key (`PRAGMA key`).
