# wigwag HTTP API, quick reference

The same API as `signal-cli daemon --http`. Default address: `http://127.0.0.1:8080`.

| Endpoint | |
|---|---|
| `GET /api/v1/check` | Returns `200` while connected, and `503` once the device has been unlinked. |
| `POST /api/v1/rpc` | JSON-RPC 2.0. Takes a single call or a batch. |
| `GET /api/v1/events` | Server-sent events: one `event: receive` per envelope. Every subscriber gets every event. While nobody is subscribed, envelopes are held (`--backlog`) for the next subscriber. |

## Calls you'll use

```sh
rpc() {  # rpc METHOD [PARAMS_JSON]
  params=${2:-'{}'}
  curl -s localhost:8080/api/v1/rpc -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$params}"
}

rpc listAccounts                                    # [{"number":"+1…"}]
rpc getContact '{"recipient":"+15551234567"}'       # {"uuid":…, "number":…}
rpc listContacts
rpc listGroups                                      # ids are base64; use them as groupId
rpc send '{"noteToSelf":true,"message":"hi me"}'
rpc send '{"recipient":["<uuid>"],"message":"hi"}'
rpc send '{"groupId":"<base64>","message":"hi all"}'
rpc sendTyping '{"recipient":["<uuid>"]}'
rpc sendReaction '{"recipient":["<uuid>"],"emoji":"👍","targetAuthor":"<uuid>","targetTimestamp":1700000000000}'
```

`send` also takes `attachments` (file paths or `data:` URIs), `quoteTimestamp` with
`quoteAuthor`, `mention`, and `textStyle`.

## Envelopes (what the event stream carries)

```jsonc
// someone else wrote, in a DM or a group
{"account":"+1…","envelope":{
  "source":"<uuid>","sourceUuid":"<uuid>","sourceNumber":"+1…","sourceName":"David",
  "timestamp":1700000000000,
  "dataMessage":{"message":"chippy hi","groupInfo":{"groupId":"<base64>"},
                 "mentions":[],"attachments":[],"quote":{…}}}}

// the account owner wrote, on another device (phone or desktop)
{"account":"+1…","envelope":{"source":"+1…",
  "syncMessage":{"sentMessage":{"message":"…",
    "destination":"+1…","destinationNumber":"+1…","destinationUuid":"<uuid>",
    "groupInfo":{"groupId":"<base64>"}}}}}
// Note to Self = a sentMessage with no groupInfo whose destination is the account itself
```

Messages from someone who hasn't shared their number arrive with a UUID only. You can
only send to a `+number` that is in your contacts, so use UUIDs everywhere.
