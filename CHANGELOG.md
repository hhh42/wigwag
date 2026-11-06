# Changelog

## 0.1.0 — unreleased

First version. Named **wigwag** after the US Army's 1860 flag code; see "Why wigwag" in the README.

- Link as a secondary device, or register a number of its own.
- signal-cli-compatible HTTP API: `/api/v1/check`, `/api/v1/rpc` (JSON-RPC 2.0, batches),
  `/api/v1/events` (SSE).
- RPC methods: `send`, `sendTyping`, `sendReaction`, `getAttachment`, `listContacts`,
  `getContact`, `listGroups`, `listAccounts`, `version`.
- Envelopes for data, Note to Self / sync-sent, edit, typing, receipt and story messages.
- Inbound attachments downloaded on arrival and pruned after a TTL; stored messages purged hourly.
- Events held for the next subscriber while none is connected.
