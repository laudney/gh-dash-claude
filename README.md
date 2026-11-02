# gh-dash-claude

Press one key on a PR or issue in [gh-dash](https://github.com/dlvhdr/gh-dash),
type an instruction, and it lands in the live Claude Code session that owns
that PR — over Claude Code's native cross-session inbox, without tmux, without
restarting the session, and without an orchestrator.

The hard part is finding the owner. Sessions start in the main checkout,
work in worktrees, and push to a fork; the PR lives upstream. gh-dash-claude
joins these identities from evidence the sessions leave behind, and sends
automatically only when exactly one session *owns* the PR:

| Owner signal | Evidence |
|---|---|
| O1 | A worktree the session worked in has HEAD = the PR head, and its work is after the worktree's last HEAD move |
| O2 | Same, through the branch's upstream (fork + branch) |
| O3 | The session pushed the PR branch to the head fork, verified by the push output |
| O4 | You chose this session for this PR before (remembered while it runs) |

Mentions, edited files, and titles only *rank* the picker. When the evidence
is not decisive, a picker opens with the best candidate on row 1, so
confirming is one Enter, and the choice is remembered.

See `docs/how-matching-works.md` for the full model.

## Install

```sh
cargo install --path .
```

Requirements: Linux, Claude Code ≥ 2.1.224 (cross-session messaging), the
GitHub CLI `gh` (authenticated), and git.

## gh-dash keybindings

`~/.config/gh-dash/config.yml`:

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

Pass only `RepoName` and the number: gh-dash does not shell-escape template
values, and titles or branch names can contain `$(…)`. Everything else is
fetched with `gh`.

## Commands

```text
gh-dash-claude <REPO> <NUMBER> [--issue] [-m TEXT | -m -] [--editor]
               [--to PID|NAME|SESSION-ID] [--pick] [--priority now|next|later]
               [--dry-run] [--explain] [--hold always|error|never]
gh-dash-claude resolve <REPO> <NUMBER> [--issue]   explain; sends nothing
gh-dash-claude map [--repo OWNER/NAME]... [--json] [--explain]
gh-dash-claude list                                sessions and their worktrees
```

`resolve`, `map`, `list`, and `--dry-run` are read-only: they never connect to
a session socket and never write a file. `map` resolves every open PR you
authored in one GitHub call — a quick way to see which session owns what.

A successful send prints `sent to <session> (msg <id>); delivery
unconfirmed`: the receiving session's own inbound controls still decide what
happens to the message. Exit codes: `0` sent or cancelled, `1` error, `2` not
decisive without a terminal, or no eligible session.
