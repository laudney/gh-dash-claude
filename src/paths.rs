//! Locations of Claude Code state and of this tool's own state.

use std::env;
use std::path::{Path, PathBuf};

/// The user's home directory, from `$HOME`.
pub fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The Claude Code configuration directory: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
pub fn config_dir() -> PathBuf {
    match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".claude"),
    }
}

/// Directory of registry entries (`<pid>.json`) and auth keys.
pub fn sessions_dir(config: &Path) -> PathBuf {
    config.join("sessions")
}

/// Directory of per-project transcript folders.
pub fn projects_dir(config: &Path) -> PathBuf {
    config.join("projects")
}

/// The transcript folder name Claude Code derives from a working
/// directory: every byte outside `[A-Za-z0-9]` becomes `-`.
pub fn project_dir_for(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}

/// Find the transcript of a session: first in the folder derived from its
/// `cwd`, then in any project folder.
pub fn transcript_path(config: &Path, cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let projects = projects_dir(config);
    let file = format!("{session_id}.jsonl");
    let direct = projects.join(project_dir_for(cwd)).join(&file);
    if direct.is_file() {
        return Some(direct);
    }
    let entries = std::fs::read_dir(&projects).ok()?;
    entries
        .flatten()
        .map(|e| e.path().join(&file))
        .find(|p| p.is_file())
}

/// This tool's state directory: `$XDG_STATE_HOME/gh-dash-claude`, else
/// `~/.local/state/gh-dash-claude`.
pub fn state_dir() -> PathBuf {
    match env::var_os("XDG_STATE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("gh-dash-claude"),
        _ => home().join(".local/state/gh-dash-claude"),
    }
}

/// Expand a leading `~` or `~/` with `home`. Other forms (`~user`) are not
/// expanded and yield `None`.
pub fn expand_tilde(word: &str, home: &Path) -> Option<PathBuf> {
    if word == "~" {
        Some(home.to_path_buf())
    } else if let Some(rest) = word.strip_prefix("~/") {
        // `Path::join` replaces the base for an absolute remainder, but the
        // shell keeps `$HOME`: `~//srv` is `$HOME/srv`.
        Some(home.join(rest.trim_start_matches('/')))
    } else if word.starts_with('~') {
        None
    } else {
        Some(PathBuf::from(word))
    }
}

/// Render a path with `$HOME` shown as `~`.
pub fn display_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dir_encoding_matches_claude_code() {
        assert_eq!(
            project_dir_for(Path::new("/home/u/src/project")),
            "-home-u-src-project"
        );
        assert_eq!(project_dir_for(Path::new("/a/b.c_d")), "-a-b-c-d");
    }

    #[test]
    fn tilde_expansion() {
        let h = Path::new("/home/u");
        assert_eq!(expand_tilde("~", h), Some(PathBuf::from("/home/u")));
        assert_eq!(expand_tilde("~/x/y", h), Some(PathBuf::from("/home/u/x/y")));
        assert_eq!(
            expand_tilde("~//srv/r", h),
            Some(PathBuf::from("/home/u/srv/r"))
        );
        assert_eq!(expand_tilde("~/", h), Some(PathBuf::from("/home/u")));
        assert_eq!(expand_tilde("~other/x", h), None);
        assert_eq!(expand_tilde("/abs", h), Some(PathBuf::from("/abs")));
    }

    #[test]
    fn home_display() {
        let h = Path::new("/home/u");
        assert_eq!(display_home(Path::new("/home/u/a"), h), "~/a");
        assert_eq!(display_home(Path::new("/srv/a"), h), "/srv/a");
    }
}
