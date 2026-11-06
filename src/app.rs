//! Command orchestration.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::{Cli, Cmd, Common, Hold, MapArgs, SendArgs};
use crate::dialog::{self, Compose, Outcome, Row};
use crate::paths;
use crate::pipeline::{Ctx, World, gather, load_sessions, rank};
use crate::proc::Liveness;
use crate::registry::{self, Session};
use crate::resolve::{Decision, Ranking};
use crate::routes::Routes;
use crate::slug::RepoSlug;
use crate::target::Target;
use crate::time::now_ms;
use crate::transport;
use crate::tui::{Interrupted, Tui};
use crate::ui;
use crate::{github, message};

/// How the process should exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok,
    /// Not decisive and no one to ask, or no eligible session.
    Undecided,
}

/// Entry point.
pub fn run(cli: Cli) -> Result<Exit> {
    match cli.cmd {
        Some(Cmd::List(c)) => list(&c),
        Some(Cmd::Map(m)) => map(&m),
        Some(Cmd::Resolve(r)) => {
            let ctx = Ctx::new(&r.common);
            let (slug, n) = r.target.validate()?;
            explain_one(&ctx, &slug, n, r.target.issue, None).map(|_| Exit::Ok)
        }
        None => send(&cli.send),
    }
}

fn explain_one(
    ctx: &Ctx,
    slug: &RepoSlug,
    n: u64,
    issue: bool,
    to: Option<&Session>,
) -> Result<()> {
    let sessions = load_sessions(ctx);
    let (targets, world) = gather(ctx, &sessions.eligible, || {
        github::fetch(slug, n, issue).map(|t| vec![t])
    })?;
    let (routes, warn) = Routes::load(&paths::state_dir());
    if let Some(w) = warn {
        eprintln!("warning: {w}");
    }
    let t = &targets[0];
    let r = rank(ctx, &world, &routes.live_view(&t.route_keys()), t);
    let mut out = std::io::stdout().lock();
    writeln!(out, "{} — {}", t.key(), t.title())?;
    if let Some(s) = to {
        writeln!(out, "  --to: {} (pid {})", s.name, s.pid)?;
    }
    ui::explain(&r, &world.ev, &ctx.home, &mut out)?;
    Ok(())
}

fn list(c: &Common) -> Result<Exit> {
    let ctx = Ctx::new(c);
    let sessions = load_sessions(&ctx);
    let (_, world) = gather(&ctx, &sessions.eligible, || Ok(Vec::new()))?;
    let mut out = std::io::stdout().lock();
    for e in &world.ev {
        let s = &e.session;
        writeln!(
            out,
            "{} [{}] pid {} · {}",
            s.name,
            s.status,
            s.pid,
            e.title().unwrap_or("(no title)")
        )?;
        let mut roots: Vec<_> = e.roots.iter().collect();
        roots.sort_by_key(|(_, t)| std::cmp::Reverse((t.work, t.reference)));
        for (root, t) in roots.into_iter().take(3) {
            let w = world.git.worktrees.get(root);
            writeln!(
                out,
                "    {} work {} ref {} · {} @ {}",
                paths::display_home(root, &ctx.home),
                t.work,
                t.reference,
                w.and_then(|w| w.branch.as_deref()).unwrap_or("-"),
                w.and_then(|w| w.head_oid.as_deref())
                    .map_or("-", |o| &o[..8.min(o.len())])
            )?;
        }
    }
    for (s, why) in &sessions.excluded {
        writeln!(out, "{} pid {} — excluded: {why}", s.name, s.pid)?;
    }
    Ok(Exit::Ok)
}

fn map(m: &MapArgs) -> Result<Exit> {
    let ctx = Ctx::new(&m.common);
    let repos: Vec<RepoSlug> = m
        .repos
        .iter()
        .map(|r| RepoSlug::parse(r).ok_or_else(|| anyhow!("invalid repository {r:?}")))
        .collect::<Result<_>>()?;
    let sessions = load_sessions(&ctx);
    let (targets, world) = gather(&ctx, &sessions.eligible, || {
        github::fetch_my_open_prs(&repos).map(|v| v.into_iter().map(Target::Pr).collect())
    })?;
    let (routes, _) = Routes::load(&paths::state_dir());
    let mut out = std::io::stdout().lock();
    let mut rows = Vec::new();
    for t in &targets {
        let r = rank(&ctx, &world, &routes.live_view(&t.route_keys()), t);
        let head = match t {
            Target::Pr(p) => p.head_ref.clone(),
            Target::Issue(_) => String::new(),
        };
        let decision = match r.decision {
            Decision::Auto(_) => "auto",
            Decision::Pick { .. } if r.top().is_some_and(|c| c.score > 0.0) => "pick",
            Decision::Pick { .. } | Decision::NoCandidates => "none",
        };
        let top = r.top().filter(|c| c.score > 0.0);
        if m.json {
            rows.push(serde_json::json!({
                "target": t.key(),
                "head": head,
                "decision": decision,
                "candidates": r.candidates.iter().map(|c| serde_json::json!({
                    "name": world.ev[c.session].session.name,
                    "pid": c.pid,
                    "score": c.score,
                    "worktree": c.worktree,
                    "signals": c.signals().map(|s| serde_json::json!({
                        "id": s.id, "points": s.points, "reason": s.reason
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }));
            continue;
        }
        writeln!(
            out,
            "{:<28} {:<34} {:<4} {}",
            t.key(),
            head,
            decision,
            top.map(|c| ui::candidate_line(c, &world.ev, &ctx.home))
                .unwrap_or_default()
        )?;
        if m.explain {
            ui::explain(&r, &world.ev, &ctx.home, &mut out)?;
        }
    }
    if m.json {
        serde_json::to_writer_pretty(&mut out, &rows)?;
        writeln!(out)?;
    }
    ctx.phase("resolve");
    Ok(Exit::Ok)
}

/// Whether `s` has the 8-4-4-4-12 hexadecimal form of a session id.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Match `--to` against eligible sessions.
fn match_to<'a>(to: &str, sessions: &'a [Session]) -> Result<&'a Session> {
    let is_uuid = is_uuid(to);
    let hits: Vec<&Session> = if to.bytes().all(|b| b.is_ascii_digit()) {
        sessions
            .iter()
            .filter(|s| s.pid.to_string() == to)
            .collect()
    } else if is_uuid {
        sessions.iter().filter(|s| s.session_id == to).collect()
    } else {
        sessions.iter().filter(|s| s.name == to).collect()
    };
    match hits.as_slice() {
        [one] => Ok(one),
        _ => {
            let names: Vec<String> = sessions
                .iter()
                .map(|s| format!("{} ({})", s.name, s.pid))
                .collect();
            bail!(
                "--to {to:?} matches {} eligible sessions; eligible: {}",
                hits.len(),
                names.join(", ")
            )
        }
    }
}

/// Set when the dialog was used, so `--hold` does not wait a second time.
static MODAL_USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn send(a: &SendArgs) -> Result<Exit> {
    let result = send_inner(a);
    let failed = !matches!(result, Ok(Exit::Ok));
    if let Err(e) = &result {
        eprintln!("error: {e:#}");
    }
    // `--dry-run` takes no input at all; the dialog has its own
    // result screen.
    let modal = MODAL_USED.load(std::sync::atomic::Ordering::SeqCst);
    // After a termination signal, never wait for input.
    match a.hold {
        _ if a.dry_run || modal || crate::tui::signalled() => {}
        Hold::Always => ui::hold(),
        Hold::Error if failed => ui::hold(),
        _ => {}
    }
    match result {
        Ok(x) => Ok(x),
        Err(_) => std::process::exit(1),
    }
}

/// Everything a send needs once resolution is done.
struct Prepared {
    target: Target,
    world: World,
    ranking: Ranking,
    routes: Routes,
    warnings: Vec<String>,
}

fn prepare(
    ctx: &Ctx,
    eligible: &[Session],
    slug: &RepoSlug,
    n: u64,
    issue: bool,
) -> Result<Prepared> {
    let (mut targets, world) = gather(ctx, eligible, || {
        github::fetch(slug, n, issue).map(|t| vec![t])
    })?;
    let target = targets.remove(0);
    let (routes, warn) = Routes::load(&paths::state_dir());
    let ranking = rank(
        ctx,
        &world,
        &routes.live_view(&target.route_keys()),
        &target,
    );
    Ok(Prepared {
        target,
        world,
        ranking,
        routes,
        warnings: warn.into_iter().collect(),
    })
}

/// What a delivery produced, printed once the terminal is restored.
struct Report {
    exit: Exit,
    out: Option<String>,
    err: Vec<String>,
}

impl Report {
    fn print(&self) {
        for e in &self.err {
            eprintln!("{e}");
        }
        if let Some(o) = &self.out {
            println!("{o}");
        }
    }
}

/// Where a confirmation question goes.
enum Asker<'a> {
    Line,
    Modal(&'a mut Tui, &'a str),
}

impl Asker<'_> {
    fn interactive(&self) -> bool {
        match self {
            Asker::Line => ui::is_interactive(),
            Asker::Modal(..) => true,
        }
    }

    fn confirm(&mut self, q: &str) -> Result<bool> {
        match self {
            Asker::Line => ui::confirm(q),
            Asker::Modal(t, title) => t.confirm(title, q),
        }
    }
}

/// Re-check the chosen process and send.
fn deliver(
    ctx: &Ctx,
    a: &SendArgs,
    prep: &mut Prepared,
    idx: usize,
    picked: bool,
    text: &str,
    asker: &mut Asker<'_>,
) -> Result<Report> {
    let chosen = prep.world.ev[idx].session.clone();
    let Some(now) = registry::load_one(&ctx.sessions_dir, chosen.pid) else {
        bail!(
            "target changed: {} (pid {}) is no longer registered; nothing sent",
            chosen.name,
            chosen.pid
        );
    };
    if now.liveness() != Liveness::Live
        || now.proc_start != chosen.proc_start
        || now.socket != chosen.socket
    {
        bail!(
            "target changed: {} (pid {}) restarted or moved its inbox; nothing sent",
            chosen.name,
            chosen.pid
        );
    }
    let mut session_id = chosen.session_id.clone();
    if now.session_id != chosen.session_id {
        if !asker.interactive() {
            return Ok(Report {
                exit: Exit::Undecided,
                out: None,
                err: vec![
                    "the session was cleared (/clear) meanwhile; not sending without confirmation"
                        .into(),
                ],
            });
        }
        if !asker.confirm("the session was cleared (/clear) meanwhile; send anyway?")? {
            return Ok(Report {
                exit: Exit::Ok,
                out: None,
                err: vec!["cancelled; nothing sent".into()],
            });
        }
        session_id = now.session_id.clone();
    }
    let socket: &Path = now
        .socket
        .as_deref()
        .context("the session has no inbox socket")?;

    let candidate = prep.ranking.candidates.iter().find(|c| c.session == idx);
    let worktree = candidate.and_then(|c| c.worktree.as_deref());
    let body = message::build(&prep.target, worktree, &ctx.home, text);
    let (frame, msg_id) = transport::encode(&body, &session_id, a.priority)?;
    let token = transport::token_for(socket, &ctx.sessions_dir);
    if let Asker::Modal(t, title) = asker {
        let _ = t.show(title, &format!("sending to {}…", chosen.name));
    }
    // The last point at which a signal still means "nothing sent" (for
    // both UIs: a signal caught by the dialog stays recorded).
    if crate::tui::signalled() {
        return Err(Interrupted.into());
    }
    let result = transport::send(socket, token.as_deref(), &frame, transport::SEND_TIMEOUT);
    let mut err = Vec::new();
    if picked
        && !matches!(result, Err(transport::SendError::NotSent(_)))
        && let Err(e) = prep.routes.record(&prep.target.key(), &now, now_ms())
    {
        err.push(format!("warning: could not remember the route: {e:#}"));
    }
    match result {
        Ok(()) => Ok(Report {
            exit: Exit::Ok,
            out: Some(format!(
                "sent to {} (msg {msg_id}); delivery unconfirmed{}",
                chosen.name,
                if token.is_none() {
                    " (no auth key: unauthenticated)"
                } else {
                    ""
                }
            )),
            err,
        }),
        Err(e) => {
            // Keep warnings in the error, so nothing prints over a dialog.
            let mut msg = e.to_string();
            for w in err {
                msg.push('\n');
                msg.push_str(&w);
            }
            bail!("{msg}")
        }
    }
}

fn send_inner(a: &SendArgs) -> Result<Exit> {
    let ctx = Ctx::new(&a.common);
    let (slug, n) = a.target.validate()?;
    let sessions = load_sessions(&ctx);
    let to =
        a.to.as_deref()
            .map(|t| match_to(t, &sessions.eligible))
            .transpose()?;
    if a.dry_run {
        explain_one(&ctx, &slug, n, a.target.issue, to)?;
        return Ok(Exit::Ok);
    }
    let issue = a.target.issue;
    // Mode order: the first matching case below wins.
    let dialog = ui::is_interactive() && !a.plain && a.message.as_deref() != Some("-");
    let direct_to = a.message.is_some() && !a.pick && to.is_some();
    if !dialog || direct_to {
        let prep = prepare(&ctx, &sessions.eligible, &slug, n, issue)?;
        return line_flow(&ctx, a, prep, to);
    }
    if a.message.is_some() && !a.pick {
        let prep = prepare(&ctx, &sessions.eligible, &slug, n, issue)?;
        if matches!(prep.ranking.decision, Decision::Auto(_)) {
            return line_flow(&ctx, a, prep, to);
        }
        return modal_flow(
            &ctx,
            a,
            Source::Ready(Box::new(prep)),
            to,
            &slug,
            n,
            &sessions.eligible,
        );
    }
    let (tx, rx) = std::sync::mpsc::channel();
    {
        // `--timing` would print over the dialog: silence it there.
        let mut ctx = ctx.clone();
        ctx.timing = false;
        let eligible = sessions.eligible.clone();
        let slug = slug.clone();
        std::thread::spawn(move || {
            let _ = tx.send(prepare(&ctx, &eligible, &slug, n, issue));
        });
    }
    modal_flow(
        &ctx,
        a,
        Source::Pending(rx),
        to,
        &slug,
        n,
        &sessions.eligible,
    )
}

/// The line-based flow (today's behaviour; also the fallback).
fn line_flow(ctx: &Ctx, a: &SendArgs, mut prep: Prepared, to: Option<&Session>) -> Result<Exit> {
    for w in &prep.warnings {
        eprintln!("warning: {w}");
    }
    let (world, ranking) = (&prep.world, &prep.ranking);
    eprintln!("{} — {}", prep.target.key(), prep.target.title());
    if a.explain {
        ui::explain(ranking, &world.ev, &ctx.home, &mut std::io::stderr().lock())?;
    }
    let (chosen_idx, picked): (Option<usize>, bool) = if let Some(s) = to {
        (world.ev.iter().position(|e| e.session.pid == s.pid), true)
    } else {
        match ranking.decision {
            Decision::NoCandidates => {
                eprintln!("no eligible Claude Code session");
                return Ok(Exit::Undecided);
            }
            Decision::Auto(i) if !a.pick => (Some(ranking.candidates[i].session), false),
            _ => {
                if !ui::is_interactive() {
                    eprintln!("not decisive; candidates:");
                    ui::explain(ranking, &world.ev, &ctx.home, &mut std::io::stderr().lock())?;
                    return Ok(Exit::Undecided);
                }
                match ui::pick(ranking, &world.ev, &ctx.home)? {
                    Some(row) => (Some(ranking.candidates[row].session), true),
                    None => {
                        eprintln!("cancelled; nothing sent");
                        return Ok(Exit::Ok);
                    }
                }
            }
        }
    };
    let Some(idx) = chosen_idx else {
        bail!("the --to session has no evidence entry");
    };
    match ranking.candidates.iter().find(|c| c.session == idx) {
        Some(c) => eprintln!("→ {}", ui::candidate_line(c, &world.ev, &ctx.home)),
        None => eprintln!(
            "→ {} (pid {})",
            world.ev[idx].session.name, world.ev[idx].session.pid
        ),
    }
    let src = match (a.message.as_deref(), a.editor) {
        (Some("-"), _) => ui::InstructionSource::Stdin,
        (Some(t), _) => ui::InstructionSource::Text(t),
        (None, true) => ui::InstructionSource::Editor,
        (None, false) => ui::InstructionSource::Prompt,
    };
    let text = ui::instruction(src)?;
    if text.is_empty() {
        eprintln!("empty instruction; nothing sent");
        return Ok(Exit::Ok);
    }
    let report = deliver(ctx, a, &mut prep, idx, picked, &text, &mut Asker::Line)?;
    report.print();
    Ok(report.exit)
}

/// How the dialog gets the resolution.
enum Source {
    Ready(Box<Prepared>),
    Pending(std::sync::mpsc::Receiver<Result<Prepared>>),
}

impl Source {
    fn wait(self) -> Result<Prepared> {
        match self {
            Source::Ready(p) => Ok(*p),
            Source::Pending(rx) => rx
                .recv()
                .map_err(|_| anyhow!("the resolution thread stopped unexpectedly"))?,
        }
    }
}

fn interrupted() -> anyhow::Error {
    anyhow!("interrupted; nothing sent")
}

/// The dialog flow.
#[allow(clippy::too_many_arguments)]
fn modal_flow(
    ctx: &Ctx,
    a: &SendArgs,
    source: Source,
    to: Option<&Session>,
    slug: &RepoSlug,
    n: u64,
    eligible: &[Session],
) -> Result<Exit> {
    let mut tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) if e.is::<Interrupted>() => return Err(interrupted()),
        // No usable terminal: the line UI.
        Err(_) => return line_flow(ctx, a, source.wait()?, to),
    };
    let title = format!("Send to Claude — {slug}#{n}");
    let prep = match source {
        Source::Ready(p) => Ok(*p),
        Source::Pending(rx) => {
            let text = format!(
                "fetching from GitHub and scanning {} sessions…",
                eligible.len()
            );
            match tui.loading(&title, &text, &rx) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    drop(tui);
                    eprintln!("cancelled; nothing sent");
                    return Ok(Exit::Ok);
                }
                Err(e) => {
                    drop(tui);
                    return Err(if e.is::<Interrupted>() {
                        interrupted()
                    } else {
                        e
                    });
                }
            }
        }
    };
    let mut prep = match prep {
        Ok(p) => p,
        Err(e) => {
            drop(tui);
            return Err(e);
        }
    };

    // The rows: every candidate, or only the pinned `--to` session.
    let indices: Vec<usize> = match to {
        Some(s) => prep
            .ranking
            .candidates
            .iter()
            .position(|c| c.pid == s.pid)
            .into_iter()
            .collect(),
        None => (0..prep.ranking.candidates.len()).collect(),
    };
    if indices.is_empty() {
        drop(tui);
        for w in &prep.warnings {
            eprintln!("warning: {w}");
        }
        eprintln!("no eligible Claude Code session");
        return Ok(Exit::Undecided);
    }
    let rows: Vec<Row> = indices
        .iter()
        .map(|&ci| {
            let c = &prep.ranking.candidates[ci];
            let e = &prep.world.ev[c.session];
            let mut details: Vec<String> = e
                .title()
                .map(|t| format!("title: {t}"))
                .into_iter()
                .collect();
            details.extend(
                c.signals()
                    .take(3)
                    .map(|s| format!("{} {:>5.1}  {}", s.id, s.points, s.reason)),
            );
            Row {
                line: ui::candidate_line(c, &prep.world.ev, &ctx.home),
                details,
            }
        })
        .collect();
    let verdict = match (to, prep.ranking.decision) {
        (Some(_), _) => "--to: pinned session".to_string(),
        (None, Decision::Auto(_)) => "decisive: one owner".to_string(),
        (None, _) => "not decisive — choose the session".to_string(),
    };
    let mut c = Compose::new(
        title.clone(),
        prep.target.title().to_string(),
        verdict,
        rows,
    );
    if let Some(m) = a.message.as_deref() {
        c.text = m.chars().collect();
        c.cursor = c.text.len();
        c.fixed_text = true;
    }
    c.needs_text = !a.editor;
    let outcome = match tui.compose(&mut c) {
        Ok(o) => o,
        Err(e) => {
            drop(tui);
            return Err(if e.is::<Interrupted>() {
                interrupted()
            } else {
                e
            });
        }
    };
    if outcome == Outcome::Cancel {
        drop(tui);
        explain_after(ctx, a, &prep);
        eprintln!("cancelled; nothing sent");
        return Ok(Exit::Ok);
    }
    let ci = indices[c.selected];
    let idx = prep.ranking.candidates[ci].session;
    let picked = dialog::picked(prep.ranking.decision, ci, a.pick, to.is_some());

    if a.editor {
        drop(tui);
        explain_after(ctx, a, &prep);
        if crate::tui::signalled() {
            return Err(interrupted());
        }
        let text = ui::instruction(ui::InstructionSource::Editor)?;
        if text.is_empty() {
            eprintln!("empty instruction; nothing sent");
            return Ok(Exit::Ok);
        }
        let report =
            deliver(ctx, a, &mut prep, idx, picked, &text, &mut Asker::Line).map_err(|e| {
                if e.is::<Interrupted>() {
                    interrupted()
                } else {
                    e
                }
            })?;
        report.print();
        return Ok(report.exit);
    }

    let text = c.instruction();
    let result = deliver(
        ctx,
        a,
        &mut prep,
        idx,
        picked,
        &text,
        &mut Asker::Modal(&mut tui, &title),
    );
    match result {
        Ok(report) => {
            let (msg, ok) = match &report.out {
                Some(o) => (o.clone(), true),
                None => (report.err.join("\n"), false),
            };
            if !msg.is_empty() {
                MODAL_USED.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = tui.message(&title, &msg, ok);
            }
            drop(tui);
            for w in &prep.warnings {
                eprintln!("warning: {w}");
            }
            explain_after(ctx, a, &prep);
            report.print();
            Ok(report.exit)
        }
        Err(e) if e.is::<Interrupted>() => {
            drop(tui);
            Err(interrupted())
        }
        Err(e) => {
            MODAL_USED.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = tui.message(&title, &format!("{e:#}"), false);
            drop(tui);
            explain_after(ctx, a, &prep);
            Err(e)
        }
    }
}

/// `--explain` in dialog mode: printed after the terminal is restored.
fn explain_after(ctx: &Ctx, a: &SendArgs, prep: &Prepared) {
    if a.explain {
        let _ = ui::explain(
            &prep.ranking,
            &prep.world.ev,
            &ctx.home,
            &mut std::io::stderr().lock(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pid: u32, id: &str, name: &str) -> Session {
        Session {
            pid,
            session_id: id.into(),
            cwd: "/".into(),
            name: name.into(),
            name_source: "derived".into(),
            kind: "interactive".into(),
            status: "idle".into(),
            proc_start: "1".into(),
            socket: Some("/x.sock".into()),
            peer_protocol: 1,
        }
    }

    #[test]
    fn to_matches_pid_session_id_or_unique_name() {
        let all = vec![
            s(10, "11111111-2222-4333-8444-555555555555", "tf-a"),
            s(11, "22222222-3333-4444-8555-666666666666", "tf-b"),
            s(12, "33333333-4444-4555-8666-777777777777", "tf-b"),
        ];
        assert_eq!(match_to("10", &all).unwrap().pid, 10);
        assert_eq!(
            match_to("22222222-3333-4444-8555-666666666666", &all)
                .unwrap()
                .pid,
            11
        );
        assert_eq!(match_to("tf-a", &all).unwrap().pid, 10);
        assert!(match_to("tf-b", &all).is_err(), "ambiguous name");
        assert!(match_to("99", &all).is_err());
        let named = vec![s(13, "x", "projectname-review-session-docs-bot0")];
        assert_eq!(
            match_to("projectname-review-session-docs-bot0", &named)
                .unwrap()
                .pid,
            13
        );
    }
}
