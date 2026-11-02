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
```

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
```

A send reports `sent to <session> (msg <id>); delivery unconfirmed`: the
receiving session's own inbound settings still decide what happens to the
message. Exit codes: `0` sent or cancelled, `1` error, `2` not decisive
without a terminal, or no eligible session.

## License

[MIT](LICENSE)
