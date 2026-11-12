#!/usr/bin/env python3
"""wigwag-bridge: an agent that answers to its name in Signal.

Reads wigwag's event stream, picks out messages that mention the agent's name
(or anything in Note to Self), runs an agent command with the recent chat as
its prompt, and posts the reply back into the same chat.

    wigwag-bridge --name Chippy --command 'claude -p'

Two of these, on two people's accounts, can talk to each other in a shared
chat. Every reply is prefixed "🤖 <Name>: ", which is how each bridge tells an
agent's message from its owner's (both are sent from the owner's account), and
an agent-to-agent run stops after --max-agent-turns until a person speaks.

Python 3.10+, standard library only. AGPL-3.0-only, like the rest of wigwag.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import json
import logging
import os
import re
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
from typing import Iterator, Optional

log = logging.getLogger("wigwag-bridge")

AGENT_LINE = re.compile(r"^🤖\s*([^:\n]{1,40}):\s?")
NO_REPLY = "NO_REPLY"

PROMPT = """\
You are {name}, an AI agent in a Signal chat, acting for {owner}. You were
mentioned by name (or messaged in {owner}'s Note to Self), so reply.

How this chat works:
- Messages from people show their name. Messages from AI agents start with
  "🤖 Name:". Other agents may be in the chat; they act for other people.
- Your reply is sent from {owner}'s Signal account with "🤖 {name}: " in front.
  Write only the message text: no prefix, no preamble, no sign-off.
- Keep it short, the way people text.
- Say another agent's name only when you need that agent to answer, because
  saying it makes that agent reply. When the exchange is done, don't name it.
- If no reply is needed, answer with exactly {no_reply} and nothing will be sent.
{extra}
Chat: {chat}
Recent messages, oldest first:
{history}

Reply to this message:
{latest}
"""


@dataclasses.dataclass
class Config:
    name: str
    command: list
    url: str = "http://127.0.0.1:8080"
    aliases: tuple = ()
    allow: frozenset = frozenset()
    owner: str = "my owner"
    note_to_self: bool = True
    max_agent_turns: int = 4
    max_replies_per_hour: int = 60
    history: int = 20
    timeout: int = 300
    instructions: str = ""
    dry_run: bool = False

    @property
    def prefix(self) -> str:
        return f"🤖 {self.name}: "


@dataclasses.dataclass
class Turn:
    """A message the agent should answer."""

    chat: str  # stable key for the conversation
    route: dict  # send params that target that conversation
    sender: str
    text: str
    agent: Optional[str]  # set when an agent wrote it
    note_to_self: bool = False


@dataclasses.dataclass
class Chat:
    lines: collections.deque
    agent_streak: int = 0  # agent messages since a person last spoke


class Bridge:
    def __init__(self, cfg: Config, own: set):
        self.cfg = cfg
        self.own = {x for x in own if x}  # our number and UUID
        names = [cfg.name, *cfg.aliases]
        self.mention = re.compile(
            r"(?<![\w@])@?(?:%s)\b" % "|".join(re.escape(n) for n in names), re.IGNORECASE
        )
        self.chats: "collections.OrderedDict[str, Chat]" = collections.OrderedDict()
        self.sent_at: collections.deque = collections.deque()

    # --- deciding ------------------------------------------------------------------------
    def chat(self, key: str) -> Chat:
        c = self.chats.get(key)
        if c is None:
            c = self.chats[key] = Chat(collections.deque(maxlen=self.cfg.history))
            while len(self.chats) > 500:
                self.chats.popitem(last=False)
        self.chats.move_to_end(key)
        return c

    def handle(self, event: dict) -> Optional[Turn]:
        """Record a message in its chat's history; return a Turn if it should be answered."""
        env = event.get("envelope", event)
        sync = (env.get("syncMessage") or {}).get("sentMessage")
        if isinstance(sync, dict):
            return self._own_message(sync)
        data = env.get("dataMessage")
        if isinstance(data, dict):
            return self._their_message(env, data)
        return None  # receipts, typing, edits, stories

    def _own_message(self, sent: dict) -> Optional[Turn]:
        """Something the owner sent from another device (their phone)."""
        text = sent.get("message") or ""
        group = (sent.get("groupInfo") or {}).get("groupId")
        dest = sent.get("destinationNumber") or sent.get("destination")
        dest_uuid = sent.get("destinationUuid")
        if not text:
            return None
        if not group and (dest in self.own or dest_uuid in self.own):
            if self._is_ours(text) or not self.cfg.note_to_self:
                return None
            key = "note-to-self"
            self._record(key, self.cfg.owner, text, None)
            return Turn(key, {"noteToSelf": True}, self.cfg.owner, text, None, True)
        target = dest_uuid or dest
        if not group and not target:
            return None
        key = f"group:{group}" if group else f"dm:{target}"
        route = {"groupId": group} if group else {"recipient": [target]}
        return self._consider(key, route, self.cfg.owner, text, owner=True)

    def _their_message(self, env: dict, data: dict) -> Optional[Turn]:
        text = data.get("message") or ""
        if not text:
            return None
        group = (data.get("groupInfo") or {}).get("groupId")
        uuid, number = env.get("sourceUuid"), env.get("sourceNumber")
        sender_ids = {uuid, number, env.get("source")} - {None, ""}
        who = env.get("sourceName") or number or uuid or "someone"
        key = f"group:{group}" if group else f"dm:{uuid or number}"
        route = {"groupId": group} if group else {"recipient": [uuid or number]}
        allowed = bool(sender_ids & self.cfg.allow)
        return self._consider(key, route, who, text, owner=False, allowed=allowed)

    def _consider(self, key, route, who, text, owner, allowed=True) -> Optional[Turn]:
        agent = self._agent_of(text)
        if agent and agent.lower() == self.cfg.name.lower():
            return None  # our own reply, echoed back
        self._record(key, who, text, agent)
        if not (owner or allowed):
            return None
        if not self.mention.search(AGENT_LINE.sub("", text, count=1)):
            return None
        c = self.chat(key)
        if agent and c.agent_streak > self.cfg.max_agent_turns:
            log.info("%s: %d agent turns in a row, waiting for a person", key, c.agent_streak - 1)
            return None
        return Turn(key, route, who, text, agent)

    def _record(self, key, who, text, agent):
        c = self.chat(key)
        if agent:
            c.lines.append(f"🤖 {agent} (agent for {who}): {AGENT_LINE.sub('', text, count=1)}")
        else:
            c.lines.append(f"{who}: {text}")
        c.agent_streak = c.agent_streak + 1 if agent else 0

    def _is_ours(self, text: str) -> bool:
        a = self._agent_of(text)
        return bool(a) and a.lower() == self.cfg.name.lower()

    @staticmethod
    def _agent_of(text: str) -> Optional[str]:
        m = AGENT_LINE.match(text)
        return m.group(1).strip() if m else None

    def over_budget(self) -> bool:
        now = time.monotonic()
        while self.sent_at and now - self.sent_at[0] > 3600:
            self.sent_at.popleft()
        return len(self.sent_at) >= self.cfg.max_replies_per_hour

    # --- answering -----------------------------------------------------------------------
    def prompt(self, turn: Turn) -> str:
        lines = list(self.chat(turn.chat).lines)
        latest = lines.pop() if lines else f"{turn.sender}: {turn.text}"
        chat = "Note to Self (private, just you and your owner)" if turn.note_to_self else turn.chat
        return PROMPT.format(
            name=self.cfg.name,
            owner=self.cfg.owner,
            no_reply=NO_REPLY,
            extra=("\n" + self.cfg.instructions.strip() + "\n") if self.cfg.instructions.strip() else "",
            chat=chat,
            history="\n".join(lines) or "(nothing earlier)",
            latest=latest,
        )

    def run_agent(self, turn: Turn) -> Optional[str]:
        env = {
            **os.environ,
            "WIGWAG_AGENT_NAME": self.cfg.name,
            "WIGWAG_CHAT": turn.chat,
            "WIGWAG_SENDER": turn.sender,
            "WIGWAG_FROM_AGENT": turn.agent or "",
        }
        try:
            out = subprocess.run(
                self.cfg.command,
                input=self.prompt(turn),
                capture_output=True,
                text=True,
                timeout=self.cfg.timeout,
                env=env,
            )
        except (OSError, subprocess.TimeoutExpired) as e:
            log.error("agent command failed: %s", e)
            return None
        if out.returncode != 0:
            log.error("agent command exited %d: %s", out.returncode, out.stderr.strip()[-500:])
            return None
        reply = out.stdout.strip()
        if not reply or reply == NO_REPLY:
            return None
        return self.cfg.prefix + AGENT_LINE.sub("", reply, count=1)

    def answer(self, turn: Turn, rpc) -> Optional[str]:
        if self.over_budget():
            log.warning("hit --max-replies-per-hour (%d); skipping", self.cfg.max_replies_per_hour)
            return None
        if turn.note_to_self:
            rpc("sendTyping", {"noteToSelf": True})
        reply = self.run_agent(turn)
        if turn.note_to_self:
            rpc("sendTyping", {"noteToSelf": True, "stop": True})
        if reply is None:
            return None
        self._record(turn.chat, self.cfg.owner, reply, self.cfg.name)
        self.sent_at.append(time.monotonic())
        if self.cfg.dry_run:
            log.info("[dry run] %s ← %s", turn.chat, reply)
        else:
            rpc("send", {**turn.route, "message": reply})
        return reply


# --- talking to wigwag -------------------------------------------------------------------
class Wigwag:
    def __init__(self, url: str):
        self.url = url.rstrip("/")
        self.ids = 0

    def rpc(self, method: str, params: Optional[dict] = None):
        self.ids += 1
        body = json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}, "id": self.ids})
        req = urllib.request.Request(
            self.url + "/api/v1/rpc", body.encode(), {"Content-Type": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=60) as r:
            resp = json.load(r)
        if "error" in resp:
            raise RuntimeError(f"{method}: {resp['error'].get('message')}")
        return resp.get("result")

    def safe_rpc(self, method: str, params: Optional[dict] = None):
        try:
            return self.rpc(method, params)
        except (OSError, RuntimeError, ValueError) as e:
            log.error("%s failed: %s", method, e)
            return None

    def own_ids(self) -> set:
        number = (self.rpc("listAccounts") or [{}])[0].get("number")
        ids = {number}
        try:
            contact = self.rpc("getContact", {"recipient": number}) or {}
            ids.add(contact.get("uuid"))
        except (OSError, RuntimeError):
            pass
        return {i for i in ids if i}

    def events(self) -> Iterator[dict]:
        """Server-sent events, reconnecting forever."""
        delay = 1
        while True:
            try:
                req = urllib.request.Request(self.url + "/api/v1/events", headers={"Accept": "text/event-stream"})
                with urllib.request.urlopen(req, timeout=90) as r:
                    log.info("connected to %s", self.url)
                    delay = 1
                    data = []
                    for raw in r:
                        line = raw.decode("utf-8", "replace").rstrip("\r\n")
                        if line.startswith("data:"):
                            data.append(line[5:].lstrip(" "))
                        elif not line and data:
                            try:
                                yield json.loads("\n".join(data))
                            except ValueError:
                                log.warning("skipping an event that isn't JSON")
                            data = []
            except (OSError, urllib.error.URLError) as e:
                log.warning("event stream: %s; retrying in %ds", e, delay)
            time.sleep(delay)
            delay = min(delay * 2, 60)


def parse_args(argv=None) -> Config:
    env = os.environ.get
    p = argparse.ArgumentParser(
        prog="wigwag-bridge",
        description="Answer to a name in Signal by running an agent command.",
    )
    p.add_argument("--name", default=env("WIGWAG_AGENT_NAME"), help="the agent's name; mentioning it triggers a reply")
    p.add_argument("--alias", action="append", default=[], help="another name it answers to (repeatable)")
    p.add_argument(
        "--command",
        default=env("WIGWAG_AGENT_COMMAND", "claude -p"),
        help="agent to run; gets the prompt on stdin, prints the reply (default: 'claude -p')",
    )
    p.add_argument("--url", default=env("WIGWAG_URL", "http://127.0.0.1:8080"))
    p.add_argument(
        "--allow",
        action="append",
        default=[x for x in (env("WIGWAG_ALLOW") or "").split(",") if x.strip()],
        help="UUID or +number of someone else who may summon the agent (repeatable). "
        "The account owner always can.",
    )
    p.add_argument("--owner", default=env("WIGWAG_OWNER", "my owner"), help="whose agent this is, by name")
    p.add_argument("--no-note-to-self", action="store_true", help="don't answer every Note to Self message")
    p.add_argument("--max-agent-turns", type=int, default=int(env("WIGWAG_MAX_AGENT_TURNS", "4")))
    p.add_argument("--max-replies-per-hour", type=int, default=60)
    p.add_argument("--history", type=int, default=20, help="recent messages given to the agent")
    p.add_argument("--timeout", type=int, default=300, help="seconds the agent may take")
    p.add_argument("--instructions", help="file with extra instructions for the agent (persona, rules)")
    p.add_argument("--dry-run", action="store_true", help="run the agent but don't send replies")
    p.add_argument("-v", "--verbose", action="store_true")
    a = p.parse_args(argv)
    if not a.name:
        p.error("--name (or WIGWAG_AGENT_NAME) is required")
    logging.basicConfig(
        level=logging.DEBUG if a.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        stream=sys.stderr,
    )
    extra = ""
    if a.instructions:
        with open(a.instructions, encoding="utf-8") as f:
            extra = f.read()
    return Config(
        name=a.name.strip(),
        command=shlex.split(a.command),
        url=a.url,
        aliases=tuple(x.strip() for x in a.alias if x.strip()),
        allow=frozenset(x.strip() for x in a.allow if x.strip()),
        owner=a.owner,
        note_to_self=not a.no_note_to_self,
        max_agent_turns=a.max_agent_turns,
        max_replies_per_hour=a.max_replies_per_hour,
        history=a.history,
        timeout=a.timeout,
        instructions=extra,
        dry_run=a.dry_run,
    )


def main(argv=None) -> int:
    cfg = parse_args(argv)
    ww = Wigwag(cfg.url)
    try:
        own = ww.own_ids()
    except (OSError, RuntimeError, ValueError) as e:
        log.error("can't reach wigwag at %s (%s). Is `wigwag daemon` running?", cfg.url, e)
        return 1
    bridge = Bridge(cfg, own)
    log.info(
        "%s is listening as %s; answers its name from the owner%s%s",
        cfg.name,
        ", ".join(sorted(own)),
        f" and {len(cfg.allow)} allowed people" if cfg.allow else " only",
        "; every Note to Self message" if cfg.note_to_self else "",
    )
    for event in ww.events():
        turn = bridge.handle(event)
        if turn:
            log.info("%s: %s mentioned %s", turn.chat, turn.agent or turn.sender, cfg.name)
            bridge.answer(turn, ww.safe_rpc)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
