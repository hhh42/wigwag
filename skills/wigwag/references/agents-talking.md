# How two agents share a Signal chat

Both agents send from **their owners' accounts**. Signal shows Chippy's messages as
coming from Harold and Jarvis's as coming from David. So the message text itself has
to say who is speaking:

```
🤖 <AgentName>: <message>
```

Every reply wigwag-bridge sends starts with that line. If the agent writes the prefix
itself, the bridge doesn't add a second one. Anything else that speaks this protocol
works with the bridge, as long as it uses the same prefix. Harold's Chippy, for
example, runs as a Hermes plugin.

## What each bridge does with a message

```
message arrives
├─ a receipt / typing / reaction / edit ............................ ignore
├─ starts with "🤖 <our name>:" ............................ our own echo, ignore
├─ record it in that chat's history (last --history lines)
│    "🤖 X:" lines count as agent turns; a person's message resets the count
├─ Note to Self (sent by the owner to their own account) ........... answer
├─ not from the owner and not from an --allow UUID ................. ignore
├─ our name (or an --alias) is not in the text after the prefix ..... ignore
├─ it's from an agent and the agent streak > --max-agent-turns ..... ignore
└─ answer: run the command with the prompt on stdin
      stdout "NO_REPLY" or empty → send nothing
      otherwise → send "🤖 <name>: <stdout>" to the same chat (DM or group)
```

"From the owner" includes messages the owner types on their phone into any chat.
wigwag gets those as sync copies, because it's linked to the same account.

## Why conversations end

Three things, in order of how often they matter:

1. **The prompt.** Each agent is told that saying another agent's name makes that
   agent reply. So it should name the other agent only when it needs an answer. A
   closing message ("Got it, thanks") without the name ends the exchange.
2. **`NO_REPLY`.** An agent that is summoned but has nothing to add says `NO_REPLY`,
   and nothing gets sent.
3. **The streak limit.** Each bridge counts agent messages in a row in each chat,
   including its own replies. Once that count is over `--max-agent-turns` (default
   4), it stops answering agents in that chat until a person writes anything there.
   Both sides enforce this on their own, so one misconfigured agent can't keep a
   loop going.

`--max-replies-per-hour` (default 60) is a separate budget, covering every chat.

## Setting up the pair

Each person does this on their own machine:

```sh
# Harold
python3 bridge/wigwag_bridge.py --name Chippy --owner Harold \
  --allow <David's ACI UUID>

# David
python3 bridge/wigwag_bridge.py --name Jarvis --owner David \
  --allow <Harold's ACI UUID>
```

To find a UUID, the other person runs `wigwag whoami` and sends you the `aci:` line.
Or look it up yourself once you've exchanged a message:

```sh
curl -s localhost:8080/api/v1/rpc -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","method":"listContacts","id":1}' | python3 -m json.tool
```

Then create a Signal group with both people in it. Agent names should be distinct
words that rarely come up in normal conversation. "Max" will fire a lot; "Jarvis"
won't.

## Tuning

| Want | Do |
|---|---|
| Longer agent-to-agent exchanges | `--max-agent-turns 8` |
| Agent only in one group | Allow nobody, and have the owner summon it there, or put the group's members in `--allow` and keep other chats DM-free. Per-group allow lists aren't built in yet. |
| A different personality, or house rules | `--instructions persona.md`. The file's text goes into every prompt. |
| More context | `--history 40`. Each prompt gets that many recent lines from the chat. |
| See what it would do without sending | `--dry-run -v` |

## Worked example: settling a plan

```
David:      jarvis can you and chippy figure out a dinner night next week
🤖 Jarvis:  Chippy, David is free Tue and Thu evenings next week. Which works for Harold?
🤖 Chippy:  Jarvis, Thursday works. Harold has a call until 6:30, so 7:30 or later.
🤖 Jarvis:  Thursday 7:30 it is. I've put it on David's calendar.
🤖 Chippy:  Added to Harold's too. Harold, David: dinner Thursday 7:30.
```

That's four agent turns, then both agents stop naming each other, so nothing more
gets triggered. For calendar access, each side's agent command needs a tool or MCP
server for it. See `setup.md` → Agent commands.
