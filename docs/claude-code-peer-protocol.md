# Claude Code's peer messaging protocol

gh-dash-claude delivers messages through the inbox that every running Claude
Code session opens for cross-session messaging (the `ListAgents` and
`SendMessage` tools). Anthropic does not publish a specification for it. This
page records the protocol as Claude Code 2.1.317 implements it on Linux, read
from the bundled JavaScript in the binary (it logs as `[uds-messaging]`) and
checked against this tool's own client. Treat it as a field guide, not a
contract: anything not listed under [What gh-dash-claude relies
on](#what-gh-dash-claude-relies-on) can change between releases.

The design has no broker and no daemon. The filesystem carries discovery and
authentication; a Unix socket per session carries delivery.

## Discovery: the session registry

Each session writes `~/.claude/sessions/<pid>.json` (under
`$CLAUDE_CONFIG_DIR` when set) and keeps it current:

```json
{
  "pid": 4242,
  "sessionId": "11111111-2222-4333-8444-555555555555",
  "cwd": "/home/u/src/project",
  "startedAt": 1793659009050,
  "procStart": "9991719",
  "pidDomain": "linux:<boot id>:pid:[4026531836]",
  "version": "2.1.317",
  "peerProtocol": 1,
  "peerFeatures": ["notify_idle", "reply_across_default_dirs", "artifact_yield"],
  "kind": "interactive",
  "entrypoint": "cli",
  "messagingSocketPath": "/run/user/1000/cc-socks/4242.sock",
  "name": "project-f7",
  "nameSource": "derived",
  "status": "shell",
  "updatedAt": 1793704963996
}
```

- **Identity** — the PID alone is not enough: a recycled PID would match a
  dead session's entry. `procStart` is the process start time from
  `/proc/<pid>/stat`, and `pidDomain` names the boot and the PID namespace.
  An entry is live only when the PID is alive *and* its start time matches.
- **Versioning** — `peerProtocol` is the wire version; `peerFeatures` lists
  optional capabilities on top of it. A client should refuse a protocol it
  does not speak and ignore features it does not use.
- **Names** — `name` is the address other sessions use. `nameSource` is
  `derived` (from the working directory), `collision` (derived, then
  disambiguated), or a source that means the user chose it.
- **Size** — Claude Code does not read a registry file of 256 KiB or more.

An entry without `messagingSocketPath` has no inbox and cannot receive.

## Transport: one Unix socket per session

The receiver binds `$XDG_RUNTIME_DIR/cc-socks/<pid>.sock`, or the same name
under its temporary directory when `$XDG_RUNTIME_DIR` is unset. When that
path is longer than the 103-byte `sun_path` budget, it falls back to
`/tmp/cc-socks-<uid>/<pid>.sock` (`$PREFIX/tmp` on Termux).
`--messaging-socket-path` overrides the location.

- **Directory vetting** — before binding, the receiver checks the ownership,
  mode, and shape of every component of the sockets directory. If any check
  fails, messaging is off for that session rather than bound somewhere
  unsafe.
- **Collisions** — if a live session from a sibling PID namespace already
  holds the path, the receiver binds `<pid>-<8 hex>.sock` instead, and later
  reaps such moved-aside sockets once they are dead. A client must therefore
  take the socket path from the registry, never construct it from the PID.
- **Children** — processes the session spawns inherit
  `CLAUDE_CODE_MESSAGING_SOCKET` and `CLAUDE_CODE_MESSAGING_TOKEN`, so a
  script can inject into its own session.
- **Gating** — the inbox is behind a server-side feature flag. A session
  that starts with it off binds later if a flag refresh turns it on.
  Restricted sessions and remote thin clients never bind.

## Authentication: the key file

On start the receiver generates a random 128-bit peer token and publishes it
in a mode-0600 file next to its registry entry:

```
~/.claude/sessions/<pid>.<sha256 of the canonical socket path>.key
{"peerToken":"<32 hex>","procStart":"…","pidDomain":"…"}
```

Naming the file after the socket path, not the PID, lets a sender find the
token from nothing but the registry entry's `messagingSocketPath`, even when
the socket was moved aside. When several key files match, the sender trusts
the one whose PID is alive with a matching `procStart`, then one that is
alive but unverifiable, and never a dead one (`transport::token_for`).
Children receive a separate child token, not the peer token.

A connection authenticates by sending this as its first line:

```json
{"type":"auth","token":"<peerToken>"}
```

Authentication is **required only on Windows**. On Linux and macOS it is
optional: the socket directory's permissions are the boundary, and a sender
that finds no key file sends without the auth line. Where it is required, a
first line that is missing, blank, unparseable, or wrong closes the
connection. On every platform, a connection that sends no complete line
within 30 s is closed.

## Framing

The connection carries newline-delimited JSON. A sender writes its lines,
half-closes its write side, and waits for the receiver to close the
connection; Claude Code's own sender gives the whole exchange 5 s. The
receiver drops an oversized line together with its connection;
gh-dash-claude keeps every frame under 1 MiB.

### User frames

A user frame puts a message into the receiving session's queue:

```json
{
  "msgV": 1,
  "msg_id": "<uuid v4>",
  "type": "user",
  "message": {
    "role": "user",
    "content": "<cross-session-message from-name=\"gh-dash\">\nRebase onto main.\n</cross-session-message>"
  },
  "priority": "next",
  "session_id": "<receiver's sessionId>"
}
```

- **`session_id`** — optional, but when present the receiver drops the frame
  unless it equals its own session id. Sending it turns a stale socket path
  (reused by a newer session) into a harmless drop instead of a misdelivery.
- **`priority`** — `now`, `next`, or `later`: where the message lands in the
  receiver's queue. Queued messages drain at the receiver's next tool round,
  so a successful send means the message reached the session, not that its
  model has read it.
- **`content`** — must be a non-empty string. Frames can also carry
  `file_attachments`, which the receiver materializes before queueing.

### The attribution envelope

The content is wrapped in an envelope that tells the receiving model who
sent it:

```
<cross-session-message from="uds:/run/user/1000/cc-socks/77.sock" from-session="…" hop-chain="…" from-name="…" from-mode="prompting" from-plugin="…">
body
</cross-session-message>
```

Every attribute is optional, but they must appear in exactly this order: the
receiver parses the envelope, rebuilds it from the parsed fields, and only
trusts the attributes if the rebuild is byte-identical. `from` is a reply
address (`uds:<socket path>`); `hop-chain` lists the sessions a relayed
message passed through, so relay loops can be dropped; `from-mode` is the
sender's permission mode (`bypass` or `prompting`). A sender must escape any
`</cross-session-message` in the body so the body cannot close the envelope
early. gh-dash-claude sends only `from-name`, because it has no inbox to
reply to.

### Control frames

Everything else is a control frame, `{"type":"control","action":"…", …}`:

| `action` | Purpose |
| --- | --- |
| `peer_message_status` | Reports what happened to a sent message, by `orig_msg_id` |
| `notify_when_idle` | Asks the receiver to report back when it next goes idle (`notify_idle`) |
| `peer_idle_notice` | The idle report that answers a `notify_when_idle` |
| `yield_artifact_replies` | Hands artifact comment replies to another live session of the same conversation (`artifact_yield`) |
| `unyield_artifact_replies` | Takes those replies back |
| `artifact_replies_yielded` | Confirms a yield |
| `rename` | Changes the session's name |

Frames that ask for an answer carry a reply address. The receiver refuses an
address outside its own socket namespace or one that points back at itself.

## Delivery status and held messages

A message can be **held**: when the sender's permission mode does not match
the receiver's (Claude Code calls the rule *permission-mode parity*), the
receiver queues it for its user's approval instead of handing it to its
model. The receiver then reports the outcome to the sender's `from`
address in `peer_message_status` frames:

| `status` | Meaning |
| --- | --- |
| `held` | Waiting for the recipient user's approval |
| `delivered` | A held message was approved and released |
| `denied` | The recipient user declined it |
| `expired` | It was held and never approved |
| `refused` | The recipient does not accept cross-session messages |
| `dropped` | The inbox dropped it: rate limit, duplicate, relay loop, or full queue (`drop_reason`, `dropped_msg_ids`) |

A sender without a reply address, like gh-dash-claude, never sees these.

## Sender failures

Claude Code's sender classifies failures as `stale_socket` (nobody listens
there), `socket_busy`, `too_large`, and `sender_paced` (it is sending too
fast). Before writing, it also checks who is on the other end of the socket,
and refuses an endpoint it cannot verify or one owned by a different
process than the registry names.

gh-dash-claude reduces failures to two outcomes that matter to a user. *Not
sent* means no byte reached the receiver, so a retry is safe. *Outcome
unknown* means bytes went out but the exchange did not finish, so the
message may or may not be in the queue and must not be resent blindly.

## What gh-dash-claude relies on

- `~/.claude/sessions/<pid>.json` with `sessionId`, `procStart`,
  `messagingSocketPath`, and `peerProtocol: 1` (`src/registry.rs`).
- The key file name scheme and its `peerToken` and `procStart` fields
  (`transport::token_for`).
- The `auth` line, the `user` frame with `msgV: 1`, and the
  `cross-session-message` envelope with `from-name` (`transport::encode`).
- Write, half-close, wait for the receiver to close, all under one 5 s
  deadline (`transport::send`).

gh-dash-claude refuses a session that advertises any other `peerProtocol`.
A change that keeps the version number would show up as messages that are
sent but never arrive, so check this page against a new Claude Code release
when that happens.
