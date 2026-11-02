//! Line-based terminal interaction. Prompts go to stderr, so that
//! stdout carries only results.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::paths::display_home;
use crate::resolve::{Candidate, Decision, Ranking};
use crate::transcript::SessionEvidence;

/// Whether the user can answer prompts.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn read_line() -> Result<Option<String>> {
    use std::io::IsTerminal;
    // Drop input that arrived before the prompt (terminal replies, focus
    // events left over from gh-dash).
    if std::io::stdin().is_terminal() {
        // SAFETY: flushes the input queue of our own stdin terminal.
        unsafe { libc::tcflush(0, libc::TCIFLUSH) };
    }
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    Ok((n > 0).then(|| sanitize(&line)))
}

/// Keep only the typed text of a line: escape sequences and terminal
/// replies are decoded and dropped.
pub fn sanitize(line: &str) -> String {
    use crate::keydec::{Decoder, Key};
    let mut d = Decoder::new();
    let mut keys = d.feed(line.as_bytes(), 0);
    keys.extend(d.tick(1_000));
    keys.into_iter()
        .filter_map(|k| match k {
            Key::Char(c) => Some(c.to_string()),
            Key::Paste(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// A one-line summary of a candidate.
pub fn candidate_line(c: &Candidate, ev: &[SessionEvidence], home: &Path) -> String {
    let e = &ev[c.session];
    let s = &e.session;
    let reason = c
        .signals()
        .next()
        .map(|sig| format!("{} {}", sig.id, sig.reason))
        .unwrap_or_else(|| "no evidence".into());
    let wt = c
        .worktree
        .as_deref()
        .map(|w| format!(" · {}", display_home(w, home)))
        .unwrap_or_default();
    format!(
        "{name} [{status}] pid {pid} score {score:.0}{wt} — {reason}",
        name = s.name,
        status = if s.status.is_empty() { "?" } else { &s.status },
        pid = s.pid,
        score = c.score,
    )
}

/// Print a ranking with every signal (`--explain`).
pub fn explain(
    r: &Ranking,
    ev: &[SessionEvidence],
    home: &Path,
    out: &mut dyn Write,
) -> Result<()> {
    let verdict = match r.decision {
        Decision::Auto(_) => "decisive: one owner",
        Decision::Pick { .. } => "not decisive: needs confirmation",
        Decision::NoCandidates => "no eligible session",
    };
    writeln!(out, "  {verdict}")?;
    for (i, c) in r.candidates.iter().enumerate() {
        let e = &ev[c.session];
        writeln!(out, "  {}) {}", i + 1, candidate_line(c, ev, home))?;
        if let Some(t) = e.title() {
            writeln!(out, "       title: {t}")?;
        }
        for s in c.signals() {
            writeln!(out, "       {:<2} {:>5.1}  {}", s.id, s.points, s.reason)?;
        }
    }
    Ok(())
}

/// Ask the user to choose; row 1 is the default. `None` means cancelled.
pub fn pick(r: &Ranking, ev: &[SessionEvidence], home: &Path) -> Result<Option<usize>> {
    let mut err = std::io::stderr().lock();
    for (i, c) in r.candidates.iter().enumerate() {
        writeln!(err, "  {}) {}", i + 1, candidate_line(c, ev, home))?;
    }
    loop {
        write!(err, "Send to [1] (number, Enter, or q): ")?;
        err.flush()?;
        let Some(line) = read_line()? else {
            return Ok(None);
        };
        let line = line.trim();
        if line.is_empty() {
            return Ok(Some(0));
        }
        if line.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        match line.parse::<usize>() {
            Ok(n) if (1..=r.candidates.len()).contains(&n) => return Ok(Some(n - 1)),
            _ => writeln!(err, "  enter 1–{}", r.candidates.len())?,
        }
    }
}

/// How the instruction is read.
pub enum InstructionSource<'a> {
    Text(&'a str),
    Stdin,
    Editor,
    Prompt,
}

/// Read the instruction; an empty result means cancel.
pub fn instruction(src: InstructionSource<'_>) -> Result<String> {
    let text = match src {
        InstructionSource::Text(t) => t.to_string(),
        InstructionSource::Stdin => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
        InstructionSource::Editor => {
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".into());
            let mut file = tempfile::Builder::new()
                .prefix("gh-dash-claude.")
                .suffix(".md")
                .tempfile()
                .context("creating the editor file")?;
            writeln!(
                file,
                "\n# Write the instruction above. Lines starting with # are dropped."
            )?;
            file.flush()?;
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{editor} \"$1\""))
                .arg("sh")
                .arg(file.path())
                .status()
                .with_context(|| format!("running {editor}"))?;
            if !status.success() {
                bail!("{editor} exited with {status}");
            }
            let text = std::fs::read_to_string(file.path())?;
            text.lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        }
        InstructionSource::Prompt => {
            let mut err = std::io::stderr().lock();
            write!(err, "> ")?;
            err.flush()?;
            read_line()?.unwrap_or_default()
        }
    };
    Ok(text.trim().to_string())
}

/// Ask a yes/no question; the default is no.
pub fn confirm(question: &str) -> Result<bool> {
    let mut err = std::io::stderr().lock();
    write!(err, "{question} [y/N]: ")?;
    err.flush()?;
    Ok(read_line()?.is_some_and(|l| l.trim().eq_ignore_ascii_case("y")))
}

/// Wait for Enter so that gh-dash does not redraw over the output.
pub fn hold() {
    if is_interactive() {
        eprint!("press Enter to return ");
        let _ = std::io::stderr().flush();
        let _ = read_line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_replies_and_keeps_text() {
        assert_eq!(sanitize("\x1b[I\x1b]11;rgb:0/0/0\x1b\\\n"), "");
        assert_eq!(sanitize("2\n"), "2");
        assert_eq!(sanitize("\x1b[O q\r\n"), " q");
    }
}
