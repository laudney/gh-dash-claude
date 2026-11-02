//! Command-line interface.

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::slug::RepoSlug;
use crate::transport::Priority;

/// Send an instruction for a GitHub PR or issue to the Claude Code session
/// that owns it.
#[derive(Debug, Parser)]
#[command(
    name = "gh-dash-claude",
    version,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
    #[command(flatten)]
    pub send: SendArgs,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Explain where a message for this PR or issue would go; sends nothing.
    Resolve(ResolveArgs),
    /// Resolve every open PR you authored; sends nothing.
    Map(MapArgs),
    /// List Claude Code sessions with the worktrees they work in.
    List(Common),
}

#[derive(Debug, Clone, Args)]
pub struct Common {
    /// Bytes of each transcript tail to scan, in MiB.
    #[arg(long, default_value_t = 8, value_name = "MIB")]
    pub tail_mib: u64,
    /// Inferred ownership expires after this many days.
    #[arg(long, default_value_t = 7, value_name = "DAYS")]
    pub max_age_days: i64,
    /// Print phase timings to stderr.
    #[arg(long)]
    pub timing: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    /// Upstream repository, `owner/name` (gh-dash `{{.RepoName}}`).
    #[arg(required = true)]
    pub repo: Option<String>,
    /// PR or issue number (gh-dash `{{.PrNumber}}` / `{{.IssueNumber}}`).
    #[arg(required = true)]
    pub number: Option<String>,
    /// The number is an issue, not a PR.
    #[arg(long)]
    pub issue: bool,
}

impl TargetArgs {
    /// Validate the only values gh-dash may interpolate.
    pub fn validate(&self) -> Result<(RepoSlug, u64)> {
        let repo = self.repo.as_deref().unwrap_or_default();
        let Some(slug) = RepoSlug::parse(repo) else {
            bail!("invalid repository {repo:?}: expected owner/name");
        };
        let number = self.number.as_deref().unwrap_or_default();
        match number.parse::<u64>() {
            Ok(n) if n > 0 && number.bytes().all(|b| b.is_ascii_digit()) => Ok((slug, n)),
            _ => bail!("invalid number {number:?}: expected a positive integer"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Hold {
    Always,
    Error,
    Never,
}

#[derive(Debug, Clone, Args)]
pub struct SendArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// The instruction; `-` reads it from stdin.
    #[arg(short, long, value_name = "TEXT")]
    pub message: Option<String>,
    /// Write the instruction in $VISUAL / $EDITOR.
    #[arg(long, conflicts_with = "message")]
    pub editor: bool,
    /// Send to this session (PID, session id, or name), skipping inference.
    #[arg(long, value_name = "SESSION")]
    pub to: Option<String>,
    /// Always confirm the target in the picker.
    #[arg(long)]
    pub pick: bool,
    #[arg(long, value_enum, default_value_t = Priority::Next)]
    pub priority: Priority,
    /// Resolve and explain, then stop: no prompt, no socket, no file.
    #[arg(long)]
    pub dry_run: bool,
    /// Print every signal of every candidate.
    #[arg(long)]
    pub explain: bool,
    /// Use line prompts instead of the dialog.
    #[arg(long)]
    pub plain: bool,
    /// Wait for Enter before exiting, so gh-dash does not hide the output
    /// (line prompts only; the dialog has its own result screen).
    #[arg(long, value_enum, default_value_t = Hold::Error)]
    pub hold: Hold,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Debug, Clone, Args)]
pub struct ResolveArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Debug, Clone, Args)]
pub struct MapArgs {
    /// Limit to these repositories (repeatable).
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    pub repos: Vec<String>,
    /// Print the rankings as JSON.
    #[arg(long)]
    pub json: bool,
    /// Print every signal of every candidate.
    #[arg(long)]
    pub explain: bool,
    #[command(flatten)]
    pub common: Common,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("gh-dash-claude").chain(args.iter().copied()))
    }

    #[test]
    fn send_is_the_default_command() {
        let c = parse(&["upstream/project", "1234", "-m", "hi"]).unwrap();
        assert!(c.cmd.is_none());
        let (s, n) = c.send.target.validate().unwrap();
        assert_eq!((s.as_str(), n), ("upstream/project", 1234));
        assert_eq!(c.send.message.as_deref(), Some("hi"));
    }

    #[test]
    fn subcommands() {
        assert!(matches!(parse(&["list"]).unwrap().cmd, Some(Cmd::List(_))));
        assert!(matches!(
            parse(&["map", "--repo", "a/b"]).unwrap().cmd,
            Some(Cmd::Map(_))
        ));
        assert!(matches!(
            parse(&["resolve", "a/b", "2", "--issue"]).unwrap().cmd,
            Some(Cmd::Resolve(_))
        ));
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn validation_rejects_shell_text() {
        let c = parse(&["a/b;rm -rf", "1"]).unwrap();
        assert!(c.send.target.validate().is_err());
        let c = parse(&["a/b", "1$(id)"]).unwrap();
        assert!(c.send.target.validate().is_err());
        let c = parse(&["a/b", "0"]).unwrap();
        assert!(c.send.target.validate().is_err());
    }
}
