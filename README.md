# gh-dash-claude

Press one key on a PR or issue in [gh-dash](https://github.com/dlvhdr/gh-dash),
type an instruction, and it lands in the running Claude Code session that
owns that PR — over Claude Code's native cross-session inbox, without tmux,
without restarting the session, and without an orchestrator.

The hard part is finding the owner: sessions start in a main checkout, work in
worktrees, and push to a fork, while the PR lives upstream. gh-dash-claude
reads the evidence sessions leave behind and sends automatically only when
exactly one session owns the PR; otherwise it asks, with the best candidate
already selected. See [`docs/how-matching-works.md`](docs/how-matching-works.md).
The inbox protocol itself is described in
[`docs/claude-code-peer-protocol.md`](docs/claude-code-peer-protocol.md).

## Requirements

- Linux
- [Claude Code](https://code.claude.com) ≥ 2.1.224 (cross-session messaging)
- [GitHub CLI](https://cli.github.com) `gh`, logged in (`gh auth status`)
- [gh-dash](https://github.com/dlvhdr/gh-dash) (`gh extension install dlvhdr/gh-dash`)
- git, and a Rust toolchain to build

## Install

```sh
cargo install --git https://github.com/laudney/gh-dash-claude
```

or from a clone: `cargo install --path .`. Check that it is on your `PATH`:

```sh
gh-dash-claude --version
```

## Set up gh-dash

**1. Open the gh-dash config.** It is usually `~/.config/gh-dash/config.yml`.

**2. Add the keybindings.** If the file has `keybindings: {}` or no
`keybindings:` key yet, replace or add:

```yaml
keybindings:
  prs:
    - key: S
      name: send to Claude
      command: gh-dash-claude '{{.RepoName}}' {{.PrNumber}}
  issues:
    - key: S
      name: send to Claude
      command: gh-dash-claude --issue '{{.RepoName}}' {{.IssueNumber}}
```

If you already have `prs:` or `issues:` entries, add these items to those
lists. `S` is free in the PR and issue views of gh-dash; pick another key if
you have bound it yourself.

Keep the command exactly in this form: pass only `{{.RepoName}}` (in single
quotes) and the number. gh-dash does not shell-escape template values, and
PR titles or branch names can contain `$(…)`; everything else is fetched with
`gh`.

**3. Restart gh-dash** (`gh dash`).

**4. Use it.** Select a PR, press `S`. A dialog opens over gh-dash:

| Key | Action |
|---|---|
| ↑ / ↓ (Ctrl+P / Ctrl+N) | choose the session (row 1 is the best match) |
| typing, paste | edit the instruction |
| Alt+Enter, Ctrl+J | newline in the instruction |
| Enter | send to the highlighted session |
| Esc, Ctrl+C | cancel — nothing is sent |

When exactly one session owns the PR it is already highlighted; when the
evidence is not decisive, check the list, and Enter confirms row 1. Your
choice is remembered for that PR while the session runs.

## Try it safely first

These commands only read; they never message a session and never write a
file:

```sh
gh-dash-claude list                          # running sessions and their worktrees
gh-dash-claude map                           # every open PR you authored → its owner
gh-dash-claude resolve owner/repo 123        # why a PR would go where it goes
gh-dash-claude owner/repo 123 --dry-run      # the same, from the send command
gh-dash-claude watch --once --dry-run        # what the watcher would forward now
```

## Forward review comments automatically

`gh-dash-claude watch` polls your open PRs and, when a maintainer or a review
bot leaves new feedback, sends it to the session that owns the PR, asking it
to assess each comment and act on it.

- **What it forwards** — conversation comments, reviews, and inline review
  comments from the base repository's owners, members and collaborators,
  plus reviews and inline comments from review bots. Your own comments and
  bot conversation comments (CI, coverage, previews) are never forwarded.
  `--from anyone` includes every human (useful when maintainers keep their
  organisation membership private); `--no-bots` leaves bots out.
- **Only when sure** — it sends only when exactly one session owns the PR,
  the same rule under which the gh-dash key sends without asking. Otherwise
  the comments wait in a queue and are retried on every poll for
  `--pending-hours` (default 24), then dropped. Picking the session once with
  the gh-dash key remembers it as an owner, which settles the common
  undecided case (no session shows evidence yet); when two sessions both
  show owner evidence, a pick does not settle it. `gh-dash-claude resolve`
  shows why a PR is undecided. After `/clear`, a session picked earlier must
  be picked again.
- **Never twice** — a comment is recorded as delivered before it is sent, so
  a crash can lose a message but never repeat one.
- **No flood** — on first start, history is ignored; `--lookback HOURS`
  includes recent comments once.
- **Limits** — each poll reads the last 50 conversation comments, the last
  30 reviews, and the first 30 inline comments of each review per PR. More
  new feedback than that between two polls can be missed; queued comments
  are kept even after they leave those windows.

Try it first; this reads GitHub, transcripts and git, and writes nothing:

```sh
gh-dash-claude watch --once --dry-run --lookback 24
```

Run it as a systemd user service:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/gh-dash-claude-watch.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now gh-dash-claude-watch
journalctl --user -u gh-dash-claude-watch -f     # what it queues and sends
```

The unit runs `~/.cargo/bin/gh-dash-claude watch`; edit `ExecStart=` to add
options or another path. The service needs `gh` logged in for your user; if
`gh` keeps its token in a keyring the service cannot reach, set `GH_TOKEN` in
an `EnvironmentFile=` (the unit shows where). The queue lives in
`~/.local/state/gh-dash-claude/watch.json`; one watcher runs at a time.

Each poll is one paginated GitHub GraphQL search, about 10 points per page
of 25 PRs out of 5,000 per hour; at the default 2-minute interval that is a
small part of the allowance. Raise `--interval` if you have hundreds of
open PRs. Rate limits are respected: the watcher waits for GitHub's retry
time.

## Troubleshooting

- **gh-dash shows "command not found"** — gh-dash runs the command through
  your shell; make sure `~/.cargo/bin` is on the `PATH` gh-dash sees, or use
  the absolute path in the `command:`.
- **"no eligible Claude Code session"** — no running session has an inbox:
  check `claude --version` (≥ 2.1.224) and `gh-dash-claude list`.
- **The wrong session is suggested** — run `gh-dash-claude resolve owner/repo
  <n>` to see every signal; choose the right row once and it is remembered.
- **The dialog misbehaves in your terminal** — add `--plain` to the
  `command:` to use line prompts instead (add `--hold always` too, so the
  result stays visible before gh-dash redraws).

## Reference

```text
gh-dash-claude <REPO> <NUMBER> [--issue] [-m TEXT | -m -] [--editor]
               [--to PID|NAME|SESSION-ID] [--pick] [--priority now|next|later]
               [--dry-run] [--explain] [--plain] [--hold always|error|never]
gh-dash-claude resolve <REPO> <NUMBER> [--issue]
gh-dash-claude map [--repo OWNER/NAME]... [--json] [--explain]
gh-dash-claude list
gh-dash-claude watch [--once] [--interval SECS] [--repo OWNER/NAME]...
                     [--from maintainers|anyone] [--no-bots]
                     [--lookback HOURS] [--pending-hours HOURS]
                     [--instruction TEXT | --instruction-file PATH]
                     [--priority now|next|later] [--dry-run]
```

A send reports `sent to <session> (msg <id>); delivery unconfirmed`: the
receiving session's own inbound settings still decide what happens to the
message. Exit codes: `0` sent or cancelled, `1` error, `2` not decisive
without a terminal, or no eligible session.

## License

[MIT](LICENSE)
