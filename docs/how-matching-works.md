# How matching works

gh-dash-claude has to answer one question: which running Claude Code session
owns this PR? Sessions do not record that. A typical session starts in the
main checkout (`~/src/project`), works in one or more worktrees
(`~/worktrees/project-pr-topic`), and pushes to a fork (`alice/project`),
while the PR lives upstream (`upstream/project`). The local branch name, the
pushed branch name, and even the pushed commit can all differ.

## Evidence

Everything is read locally, without touching any session socket, plus one
GitHub call:

- **Registry** — `~/.claude/sessions/<pid>.json`. A session counts only if
  its PID is alive *and* its kernel start time matches the entry, so a
  recycled PID is never mistaken for a session.
- **Transcript tail** — the last 8 MiB of each session's transcript. Tool
  calls show what the session really did: which files it edited, which git
  commands it ran in which worktree, and what those commands printed.
- **Git** — for every worktree a session touched: HEAD, branch, upstream,
  remotes, and the time of the last HEAD move (from the reflog). All git
  calls are read-only and take no locks.
- **GitHub** — the PR's base repo, head repo and branch, head commit, title,
  and changed files.

## What counts as proof

The Bash commands in a transcript are read conservatively. A git command
counts only when its success is *proven*: it is chained with `&&` so that the
call's exit status covers it, or its own output proves it (a push's
`old..new  src -> dst` line, a commit's `[branch abc1234]` line). Its
directory must be proven too (`cd dir && …`, `git -C dir …`). Anything the
reader cannot model — command substitutions, `eval`, unknown builtins,
background launches, variables in paths — gives no proof at all.

## Owner signals

Only these can send a message without asking:

| Signal | Rule |
|---|---|
| O1 head | A worktree that belongs to the PR (a remote is the base or head repo) has HEAD = the PR head commit, and the session worked there *after* the worktree's last HEAD move. |
| O2 track | The same, through the worktree branch's upstream (fork + branch). |
| O3 publish | The session pushed the PR branch to the head repo, proven by the push output. This links a renamed or rebased push branch. |
| O4 route | You chose this session for this PR before, and that process still runs. |

Owner evidence expires after 7 days. Work from before another actor moved a
worktree's HEAD does not count for the new state.

## Hints

Hints only rank the list:

- **M** — mentions of the PR URL, `repo#n`, and the head branch, as this
  session's *share* of all sessions' mentions (so a status session that lists
  every PR does not win).
- **F** — overlap between files the session edited and the PR's files.
- **T** — shared words between the PR title and the session title.
- **W** — the worktree matches, but the work is too old or predates the last
  HEAD move.

## Decision

- **Exactly one session with an owner signal, and no remembered route to a
  different session** → send to it.
- **Otherwise** → the picker shows every session, best first, with row 1
  selected; Enter confirms, and the choice is remembered for this PR.
- An issue uses the owners of its open linked PRs.

## Delivery

Right before sending, the chosen process is checked again (same PID, same
start time, same inbox). The message goes over Claude Code's own inbox socket
with the session id stamped, so a stale entry cannot receive it. A clean send
means "accepted by the inbox", not "acted on"; a send is never retried.

`list`, `resolve`, `map`, and `--dry-run` never open a socket and never write
a file.

## Known limits

The reader cannot see a push or commit whose output was hidden (`-q`,
redirected to a file) — such work simply gives no owner signal, and the
picker asks instead. Two actors running the same git command in the same
worktree within the same second, or a remote repointed to another repository
after a push, can still mislead it; the picker always names the session it
will send to.
