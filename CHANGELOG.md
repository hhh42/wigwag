# Changelog

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
