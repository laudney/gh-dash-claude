//! A conservative reader of the `Bash` commands found in transcripts.
//!
//! It does not interpret shell. It proves only what a successful exit
//! status implies — which directory a `git` invocation ran in, and whether
//! it ran at all — and marks everything it cannot prove as *opaque*.
//! An opaque invocation never yields ownership evidence.

use std::path::{Path, PathBuf};

use crate::paths::expand_tilde;

/// The separator that follows a simple command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    AndAnd,
    OrOr,
    /// `;` or a newline.
    Semi,
    /// `|` or `|&`.
    Pipe,
    /// A background `&`.
    Amp,
    /// End of the command text.
    End,
}

/// One shell word after quote removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    /// The word holds an expansion this reader does not model (variable,
    /// glob, brace) or is a command that runs hidden code.
    pub opaque: bool,
    /// The word starts with an unquoted `~` (the shell expands it).
    pub tilde: bool,
}

/// One simple command and the separator after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simple {
    pub words: Vec<Word>,
    pub sep_after: Sep,
    /// The command carries a redirection (which can make it fail before
    /// it runs).
    pub redirect: bool,
}

/// The lexed form of a whole `Bash` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexed {
    pub simples: Vec<Simple>,
    /// Something in the call can run commands this reader cannot see
    /// (command substitution, `eval`, a nested shell, …): every invocation
    /// of a tainted call is opaque.
    pub tainted: bool,
}

#[derive(Default)]
struct WordBuf {
    text: String,
    opaque: bool,
    started: bool,
    tilde: bool,
    /// A quote or escape occurred before the first `/`: a leading `~` is
    /// then literal.
    quoted_prefix: bool,
}

impl WordBuf {
    fn note_quote(&mut self) {
        if !self.text.contains('/') {
            self.quoted_prefix = true;
        }
    }
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    simples: Vec<Simple>,
    words: Vec<Word>,
    cur: WordBuf,
    tainted: bool,
    /// The next completed word is a redirection target, not an argument.
    skip_next_word: bool,
    /// The current simple command has a redirection.
    redirect: bool,
    /// Heredoc delimiters waiting for the end of the current line:
    /// (delimiter, strip leading tabs, delimiter was quoted).
    heredocs: Vec<(String, bool, bool)>,
}

/// Split a command into simple commands with their separators.
pub fn split(cmd: &str) -> Lexed {
    let mut lx = Lexer {
        chars: cmd.chars().peekable(),
        simples: Vec::new(),
        words: Vec::new(),
        cur: WordBuf::default(),
        tainted: false,
        skip_next_word: false,
        redirect: false,
        heredocs: Vec::new(),
    };
    lx.run();
    Lexed {
        simples: lx.simples,
        tainted: lx.tainted,
    }
}

impl Lexer<'_> {
    fn run(&mut self) {
        while let Some(c) = self.chars.next() {
            match c {
                ' ' | '\t' => self.end_word(),
                '\n' => self.newline(),
                '\\' => match self.chars.next() {
                    Some('\n') => {}
                    Some(n) => {
                        self.cur.note_quote();
                        self.cur.started = true;
                        self.cur.text.push(n);
                    }
                    None => {}
                },
                '\'' => {
                    self.cur.note_quote();
                    self.cur.started = true;
                    for n in self.chars.by_ref() {
                        if n == '\'' {
                            break;
                        }
                        self.cur.text.push(n);
                    }
                }
                '"' => self.double_quoted(),
                '`' => {
                    self.tainted = true;
                    self.push(c);
                }
                '$' => {
                    // Unquoted, even `$(cat <<'EOF' …)` is word-split and
                    // globbed, so only the double-quoted form is literal.
                    // `$(…)` runs commands; `$[…]` evaluates arithmetic,
                    // which can assign variables.
                    if matches!(self.chars.peek(), Some('(' | '[')) {
                        self.tainted = true;
                    }
                    self.cur.opaque = true;
                    self.push(c);
                }
                '*' | '?' | '[' | '{' => {
                    self.cur.opaque = true;
                    self.push(c);
                }
                '#' if !self.cur.started => {
                    while let Some(&n) = self.chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        self.chars.next();
                    }
                }
                '&' => {
                    if self.chars.peek() == Some(&'&') {
                        self.chars.next();
                        self.end_simple(Sep::AndAnd);
                    } else if self.chars.peek() == Some(&'>') {
                        // `&>file` / `&>>file`: redirect both streams.
                        self.end_word();
                        self.redirect = true;
                        self.chars.next();
                        if self.chars.peek() == Some(&'>') {
                            self.chars.next();
                        }
                        self.skip_next_word = true;
                    } else {
                        self.end_simple(Sep::Amp);
                    }
                }
                '|' => {
                    if self.chars.peek() == Some(&'|') {
                        self.chars.next();
                        self.end_simple(Sep::OrOr);
                    } else {
                        if self.chars.peek() == Some(&'&') {
                            self.chars.next();
                        }
                        self.end_simple(Sep::Pipe);
                    }
                }
                ';' => self.end_simple(Sep::Semi),
                '>' | '<' => self.redirect(c),
                '(' | ')' => {
                    // Subshells and groups change scope in ways this reader
                    // does not model.
                    self.tainted = true;
                    self.end_word();
                }
                _ => self.push(c),
            }
        }
        self.end_simple(Sep::End);
    }

    fn push(&mut self, c: char) {
        if !self.cur.started && c == '~' {
            self.cur.tilde = true;
        }
        self.cur.started = true;
        self.cur.text.push(c);
    }

    fn double_quoted(&mut self) {
        self.cur.note_quote();
        self.cur.started = true;
        while let Some(n) = self.chars.next() {
            match n {
                '"' => return,
                '\\' => match self.chars.peek() {
                    Some('"' | '\\' | '$' | '`') => {
                        let e = self.chars.next().unwrap();
                        self.cur.text.push(e);
                    }
                    Some('\n') => {
                        self.chars.next();
                    }
                    _ => self.cur.text.push('\\'),
                },
                '`' => {
                    self.tainted = true;
                    self.cur.text.push(n);
                }
                '$' => {
                    if let Some(body) = self.cat_heredoc() {
                        self.cur.text.push_str(&body);
                        continue;
                    }
                    // `$(…)` runs commands; `$[…]` evaluates arithmetic,
                    // which can assign variables.
                    if matches!(self.chars.peek(), Some('(' | '[')) {
                        self.tainted = true;
                    }
                    self.cur.opaque = true;
                    self.cur.text.push(n);
                }
                _ => self.cur.text.push(n),
            }
        }
    }

    /// Recognise the pure substitution `$(cat <<'EOF' … EOF)` that agents
    /// use for commit messages, right after its `$`. The delimiter must be
    /// quoted, so the body is literal text. On a match, consume it and
    /// return the body (trailing newlines removed, as `$(…)` does);
    /// otherwise leave the input untouched.
    fn cat_heredoc(&mut self) -> Option<String> {
        let mut it = self.chars.clone();
        let skip_blanks = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
            while matches!(it.peek(), Some(' ' | '\t')) {
                it.next();
            }
        };
        if it.next()? != '(' {
            return None;
        }
        skip_blanks(&mut it);
        for want in "cat".chars() {
            if it.next()? != want {
                return None;
            }
        }
        skip_blanks(&mut it);
        if it.next()? != '<' || it.next()? != '<' {
            return None;
        }
        let strip_tabs = it.peek() == Some(&'-');
        if strip_tabs {
            it.next();
        }
        skip_blanks(&mut it);
        let quote = it.next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        let mut delim = String::new();
        loop {
            let c = it.next()?;
            if c == quote {
                break;
            }
            delim.push(c);
        }
        if delim.is_empty() {
            return None;
        }
        skip_blanks(&mut it);
        if it.next()? != '\n' {
            return None;
        }
        let mut body = String::new();
        let mut line = String::new();
        loop {
            match it.next()? {
                '\n' => {
                    let l = if strip_tabs {
                        line.trim_start_matches('\t')
                    } else {
                        &line
                    };
                    if l == delim {
                        break;
                    }
                    body.push_str(l);
                    body.push('\n');
                    line.clear();
                }
                c => line.push(c),
            }
        }
        while matches!(it.peek(), Some(' ' | '\t' | '\n')) {
            it.next();
        }
        if it.next()? != ')' {
            return None;
        }
        self.chars = it;
        Some(body.trim_end_matches('\n').to_string())
    }

    /// Read a redirection operator. A word made only of digits right before
    /// it is the file descriptor, not an argument.
    fn redirect(&mut self, first: char) {
        self.redirect = true;
        if self.cur.started && !self.cur.opaque && self.cur.text.bytes().all(|b| b.is_ascii_digit())
        {
            self.cur = WordBuf::default();
        } else {
            self.end_word();
        }
        let mut heredoc = false;
        let mut strip_tabs = false;
        match (first, self.chars.peek().copied()) {
            ('>', Some('>' | '|')) => {
                self.chars.next();
            }
            ('<', Some('<')) => {
                self.chars.next();
                if self.chars.peek() == Some(&'<') {
                    // `<<<word`: a here-string; its word is a target.
                    self.chars.next();
                } else {
                    heredoc = true;
                    if self.chars.peek() == Some(&'-') {
                        self.chars.next();
                        strip_tabs = true;
                    }
                }
            }
            ('<', Some('>')) => {
                self.chars.next();
            }
            _ => {}
        }
        if !heredoc && self.chars.peek() == Some(&'&') {
            // `>&2`, `2>&1`, `<&-`: duplicate a descriptor; no target word.
            self.chars.next();
            while let Some(&n) = self.chars.peek() {
                if n.is_ascii_digit() || n == '-' {
                    self.chars.next();
                } else {
                    break;
                }
            }
            return;
        }
        if heredoc {
            while matches!(self.chars.peek(), Some(' ' | '\t')) {
                self.chars.next();
            }
            let mut delim = String::new();
            let mut quoted = false;
            while let Some(&n) = self.chars.peek() {
                match n {
                    '\'' => {
                        quoted = true;
                        self.chars.next();
                        for c in self.chars.by_ref() {
                            if c == '\'' {
                                break;
                            }
                            delim.push(c);
                        }
                    }
                    '"' => {
                        quoted = true;
                        self.chars.next();
                        while let Some(c) = self.chars.next() {
                            match c {
                                '"' => break,
                                // Inside double quotes, bash removes a
                                // backslash only before these characters.
                                '\\' => match self.chars.peek() {
                                    Some('$' | '`' | '"' | '\\' | '\n') => {
                                        let e = self.chars.next().unwrap();
                                        delim.push(e);
                                    }
                                    _ => delim.push('\\'),
                                },
                                _ => delim.push(c),
                            }
                        }
                    }
                    '\\' => {
                        quoted = true;
                        self.chars.next();
                        if let Some(e) = self.chars.next() {
                            delim.push(e);
                        }
                    }
                    ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                    '$' | '`' => {
                        // An expansion in a delimiter: unsupported.
                        self.tainted = true;
                        delim.push(n);
                        self.chars.next();
                    }
                    _ => {
                        delim.push(n);
                        self.chars.next();
                    }
                }
            }
            if delim.is_empty() && !quoted {
                // A heredoc without a delimiter is a syntax error.
                self.tainted = true;
            } else {
                self.heredocs.push((delim, strip_tabs, quoted));
            }
            return;
        }
        self.skip_next_word = true;
    }

    fn newline(&mut self) {
        self.end_simple(Sep::Semi);
        let pending = std::mem::take(&mut self.heredocs);
        for (delim, strip_tabs, quoted) in pending {
            let mut line = String::new();
            loop {
                match self.chars.next() {
                    None => return,
                    Some('\n') => {
                        let l = if strip_tabs {
                            line.trim_start_matches('\t')
                        } else {
                            &line
                        };
                        if l == delim {
                            break;
                        }
                        if !quoted && (l.contains("$(") || l.contains('`')) {
                            self.tainted = true;
                        }
                        line.clear();
                    }
                    Some(c) => line.push(c),
                }
            }
        }
    }

    fn end_word(&mut self) {
        if !self.cur.started {
            return;
        }
        let w = std::mem::take(&mut self.cur);
        if self.skip_next_word {
            self.skip_next_word = false;
            return;
        }
        // A bare `[`, `[[`, `]`, or `]]` is the test command or its end, not
        // a glob.
        let test_word = matches!(w.text.as_str(), "[" | "[[" | "]" | "]]");
        self.words.push(Word {
            opaque: w.opaque && !test_word,
            text: w.text,
            tilde: w.tilde && !w.quoted_prefix,
        });
    }

    fn end_simple(&mut self, sep: Sep) {
        self.end_word();
        self.skip_next_word = false;
        let redirect = std::mem::take(&mut self.redirect);
        if self.words.is_empty() && redirect {
            // A redirection-only command (`< /dev/null`) still runs and
            // sets the exit status.
            self.simples.push(Simple {
                words: Vec::new(),
                sep_after: sep,
                redirect,
            });
            return;
        }
        if self.words.is_empty() {
            // A separator without a command (such as a blank line) only
            // updates the separator of the previous simple command when it
            // is stronger than a plain line break.
            if let Some(last) = self.simples.last_mut()
                && sep != Sep::Semi
                && sep != Sep::End
            {
                last.sep_after = sep;
            }
            return;
        }
        let words = std::mem::take(&mut self.words);
        self.simples.push(Simple {
            words,
            sep_after: sep,
            redirect,
        });
    }
}

/// A HEAD-moving git verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveVerb {
    Commit,
    Rebase,
    CherryPick,
    Merge,
    Am,
    Reset,
    Checkout,
}

impl MoveVerb {
    /// Whether a reflog message records this verb. `switch` and
    /// `checkout` both log `checkout`.
    pub fn matches_reflog(self, message: &str) -> bool {
        let prefix = match self {
            MoveVerb::Commit => "commit",
            MoveVerb::Rebase => "rebase",
            MoveVerb::CherryPick => "cherry-pick",
            MoveVerb::Merge => "merge",
            MoveVerb::Am => "am",
            MoveVerb::Reset => "reset",
            MoveVerb::Checkout => "checkout",
        };
        message.starts_with(prefix)
    }
}

/// One `git` invocation found in a `Bash` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCall {
    /// The execution directory, when it is proven.
    pub dir: Option<PathBuf>,
    pub sub: String,
    pub args: Vec<String>,
    /// No evidence may be taken from this invocation.
    pub opaque: bool,
    /// A successful `Bash` result proves this invocation ran and succeeded.
    pub chain_verified: bool,
    /// Operands of the unconditional `cd`s this directory relies on. The
    /// directory holds only if the call's output has no `cd: <operand>:`
    /// error line (checked by the transcript scanner).
    pub assumes_cd: Vec<String>,
}

/// The parts of a `git push` relevant to publications.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushParts {
    pub remote: Option<String>,
    /// Explicit destination branch names, `refs/heads/` stripped.
    pub dests: Vec<String>,
    pub dry_run: bool,
    pub delete: bool,
    /// `--tags`, `--mirror`, or `--all`: no single-branch evidence.
    pub bulk: bool,
    /// An option this reader does not know: no publication evidence.
    pub unsupported: bool,
}

/// Environment keys that can change which repository or remote git uses.
fn is_context_key(k: &str) -> bool {
    if k == "HOME" || k == "XDG_CONFIG_HOME" {
        return true;
    }
    let Some(rest) = k.strip_prefix("GIT_") else {
        return false;
    };
    !(rest.starts_with("AUTHOR_")
        || rest.starts_with("COMMITTER_")
        || matches!(
            rest,
            "EDITOR" | "SEQUENCE_EDITOR" | "PAGER" | "TERMINAL_PROMPT"
        ))
}

/// Split `K=V` into its key when `K` is a shell identifier.
fn assignment_key(w: &str) -> Option<&str> {
    let (k, _) = w.split_once('=')?;
    let k = k.strip_suffix('+').unwrap_or(k);
    let mut b = k.bytes();
    let first = b.next()?;
    ((first.is_ascii_alphabetic() || first == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_'))
    .then_some(k)
}

/// Lexically normalise `.` and `..` the way bash's logical `cd` does.
fn normalize_logical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The target of `cd <word>`, when it is certain. A relative target must
/// start with `./` or `../` (or be `.`/`..`): only then does bash ignore
/// `CDPATH`.
fn cd_target(base: Option<&Path>, w: &Word, home: &Path) -> Option<PathBuf> {
    if w.opaque {
        return None;
    }
    let p = if w.tilde {
        expand_tilde(&w.text, home)?
    } else {
        PathBuf::from(&w.text)
    };
    if p.is_absolute() {
        return Some(normalize_logical(&p));
    }
    let t = w.text.as_str();
    let explicit = t == "." || t == ".." || t.starts_with("./") || t.starts_with("../");
    if !explicit {
        return None;
    }
    base.map(|b| normalize_logical(&b.join(p)))
}

/// The directory of `git -C <word>`: git uses a physical `chdir`, so a
/// `..` component (which could cross a symlink) is not resolved.
fn dash_c_target(base: Option<&Path>, w: &Word, home: &Path) -> Option<PathBuf> {
    if w.opaque {
        return None;
    }
    let p = if w.tilde {
        expand_tilde(&w.text, home)?
    } else {
        PathBuf::from(&w.text)
    };
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return None;
    }
    if p.is_absolute() {
        Some(p)
    } else {
        base.map(|b| b.join(p))
    }
}

/// How a simple command's program runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runs {
    /// In the current shell (no wrapper, or `command` / `builtin`).
    Shell,
    /// In a child process (`env`, `nohup`, `timeout`, …): it cannot change
    /// the shell's directory or variables.
    Child,
}

/// Shell builtins and keywords that are harmless here: they cannot change
/// the directory, variables, or control flow of the calling shell.
fn harmless_builtin(prog: &str) -> bool {
    matches!(
        prog,
        "echo"
            | "true"
            | "false"
            | "test"
            | "["
            | "[["
            | ":"
            | "pwd"
            | "type"
            | "help"
            | "history"
            | "jobs"
            | "times"
            | "kill"
            | "wait"
            | "dirs"
    )
}

/// Every bash builtin and reserved word. Any of them that is neither
/// modelled nor harmless taints the call: only these can change the state
/// of the calling shell.
fn is_builtin(prog: &str) -> bool {
    matches!(
        prog,
        "alias"
            | "bg"
            | "bind"
            | "break"
            | "builtin"
            | "caller"
            | "cd"
            | "command"
            | "compgen"
            | "complete"
            | "compopt"
            | "continue"
            | "declare"
            | "dirs"
            | "disown"
            | "echo"
            | "enable"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "false"
            | "fc"
            | "fg"
            | "getopts"
            | "hash"
            | "help"
            | "history"
            | "jobs"
            | "kill"
            | "let"
            | "local"
            | "logout"
            | "mapfile"
            | "popd"
            | "printf"
            | "pushd"
            | "pwd"
            | "read"
            | "readarray"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "shopt"
            | "source"
            | "suspend"
            | "test"
            | "times"
            | "trap"
            | "true"
            | "type"
            | "typeset"
            | "ulimit"
            | "umask"
            | "unalias"
            | "unset"
            | "wait"
            | "."
            | ":"
            | "["
            | "[["
            | "]]"
            | "if"
            | "then"
            | "else"
            | "elif"
            | "fi"
            | "case"
            | "esac"
            | "for"
            | "select"
            | "while"
            | "until"
            | "do"
            | "done"
            | "in"
            | "function"
            | "{"
            | "}"
            | "!"
            | "coproc"
            | "time"
    ) || matches!(prog, "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish")
}

/// `set` arguments that keep the proofs sound (errexit, nounset, xtrace,
/// pipefail only make failures louder).
fn safe_set_args(args: &[Word]) -> bool {
    let mut i = 0;
    while let Some(w) = args.get(i) {
        let t = w.text.as_str();
        if w.opaque {
            return false;
        }
        if t == "-o" || t == "+o" {
            match args.get(i + 1).map(|w| w.text.as_str()) {
                Some("pipefail" | "errexit" | "nounset" | "xtrace") => i += 2,
                _ => return false,
            }
            continue;
        }
        let flags = t.strip_prefix('-').or_else(|| t.strip_prefix('+'));
        match flags {
            Some(f) if !f.is_empty() && f.chars().all(|c| matches!(c, 'e' | 'u' | 'x')) => i += 1,
            Some(f)
                if f.chars().all(|c| matches!(c, 'e' | 'u' | 'x' | 'o')) && f.ends_with('o') =>
            {
                match args.get(i + 1).map(|w| w.text.as_str()) {
                    Some("pipefail" | "errexit" | "nounset" | "xtrace") => i += 2,
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    true
}

/// Skip leading assignments and wrappers. Returns the program word index,
/// whether the prefix makes a git invocation opaque, and where the program
/// runs.
fn program_index(words: &[Word]) -> (usize, bool, Runs) {
    let mut idx = 0;
    let mut opaque = false;
    let mut runs = Runs::Shell;
    while let Some(w) = words.get(idx) {
        match assignment_key(&w.text) {
            Some(k) => {
                if is_context_key(k) || k == "CDPATH" || w.opaque {
                    opaque = true;
                }
                idx += 1;
            }
            None => break,
        }
    }
    while let Some(w) = words.get(idx) {
        match w.text.as_str() {
            "command" | "builtin" => {
                let wrapper = w.text.as_str();
                idx += 1;
                while let Some(o) = words.get(idx) {
                    match o.text.as_str() {
                        "-p" if wrapper == "command" => idx += 1,
                        "--" => {
                            idx += 1;
                            break;
                        }
                        // `command -v`/`-V` only looks a name up.
                        t if t.starts_with('-') => {
                            return (words.len(), opaque, runs);
                        }
                        _ => break,
                    }
                }
            }
            "env" | "nohup" | "nice" | "timeout" | "stdbuf" | "ionice" | "setsid" | "chrt"
            | "taskset" | "unbuffer" | "noglob" => {
                let wrapper = w.text.as_str();
                runs = Runs::Child;
                idx += 1;
                while let Some(o) = words.get(idx) {
                    if let Some(k) = assignment_key(&o.text) {
                        if is_context_key(k) || o.opaque {
                            opaque = true;
                        }
                        idx += 1;
                    } else if o.text.starts_with('-') {
                        // Options such as `env -C dir` change context.
                        opaque = true;
                        let takes_value = matches!(
                            (wrapper, o.text.as_str()),
                            ("env", "-C" | "-u" | "-S" | "--chdir" | "--unset")
                                | ("nice", "-n")
                                | ("timeout", "-s" | "-k" | "--signal" | "--kill-after")
                                | ("stdbuf", "-i" | "-o" | "-e")
                                | ("ionice", "-c" | "-n" | "-p")
                        );
                        idx += if takes_value { 2 } else { 1 };
                    } else if wrapper == "timeout"
                        && o.text.bytes().next().is_some_and(|b| b.is_ascii_digit())
                    {
                        idx += 1; // the duration
                    } else {
                        break;
                    }
                }
            }
            "sudo" | "xargs" | "doas" | "parallel" => {
                // Unknown arguments or user: no evidence from what follows.
                return (idx, true, Runs::Child);
            }
            _ => break,
        }
    }
    (idx, opaque, runs)
}

/// Find every `git` invocation of a `Bash` call with its proven directory
/// and verification status.
pub fn git_invocations(cmd: &str, home: &Path) -> Vec<GitCall> {
    git_invocations_with(cmd, home, &searchable_dir)
}

/// Whether `p` is a directory this process may `cd` into.
fn searchable_dir(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !p.is_dir() {
        return false;
    }
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path; access only checks it.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// [`git_invocations`] with an injectable directory check.
///
/// Directory rules: the directory at the start of a call is unknown. A
/// *conditional* `cd` (one that follows `&&`) sets the directory only for
/// the commands reached from it through `&&`. An *unconditional* `cd` (the
/// first command, or one after `;` or a newline, with no `||` or `&`
/// before it), with exactly one operand, no option, and no redirection,
/// can fail only when its target is not a searchable directory; when the
/// target is one, it stays in effect through `;`, newlines, and pipes until
/// another `cd`. A `cd` inside a pipeline runs in a subshell and has no
/// effect. `pushd`/`popd` make the directory unknown.
pub fn git_invocations_with(
    cmd: &str,
    home: &Path,
    dir_exists: &dyn Fn(&Path) -> bool,
) -> Vec<GitCall> {
    let lexed = split(cmd);
    let n = lexed.simples.len();
    let progs: Vec<(usize, bool, Runs)> = lexed
        .simples
        .iter()
        .map(|s| program_index(&s.words))
        .collect();
    let tainted = lexed.tainted
        || lexed
            .simples
            .iter()
            .zip(&progs)
            .any(|(s, &(idx, _, runs))| simple_taints(&s.words, idx, runs));

    let mut out = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let mut sticky = false;
    // Sticky `cd`s the current directory relies on: (simple index, operand).
    let mut assumes: Vec<(usize, String)> = Vec::new();
    let mut seen_or_amp = false;
    let mut taint_rest = false;

    for (i, simple) in lexed.simples.iter().enumerate() {
        let words = &simple.words;
        let (idx, prefix_opaque, runs) = progs[i];
        // The call's success proves this command ran and succeeded only
        // when it is reached and followed solely through `&&`, and the call
        // does not end in the background or in a pipe.
        let chain_ok = !seen_or_amp
            && lexed.simples[i..].iter().enumerate().all(|(k, s)| {
                if i + k == n - 1 {
                    matches!(s.sep_after, Sep::End | Sep::Semi)
                } else {
                    s.sep_after == Sep::AndAnd
                }
            });
        let opaque = tainted || taint_rest || prefix_opaque;
        let prev_sep = i.checked_sub(1).map(|j| lexed.simples[j].sep_after);
        let prog = words.get(idx).map(|w| w.text.as_str());

        // Assignment-only commands change the shell's own variables.
        if prog.is_none()
            && words.iter().any(|w| {
                assignment_key(&w.text).is_some_and(|k| is_context_key(k) || k == "CDPATH")
            })
        {
            taint_rest = true;
        }

        match (prog, runs) {
            (Some("cd"), Runs::Shell) => {
                let in_pipeline = prev_sep == Some(Sep::Pipe) || simple.sep_after == Sep::Pipe;
                let operands = &words[idx + 1..];
                let target = match operands {
                    [a] if !seen_or_amp
                        && !in_pipeline
                        && !prefix_opaque
                        && !a.text.starts_with('-') =>
                    {
                        cd_target(dir.as_deref(), a, home)
                    }
                    _ => None,
                };
                let unconditional = matches!(prev_sep, None | Some(Sep::Semi));
                sticky =
                    unconditional && !simple.redirect && target.as_deref().is_some_and(dir_exists);
                if target.is_none() {
                    assumes.clear();
                } else if sticky {
                    // Only the output can prove that this `cd` succeeded.
                    // bash reports the operand after tilde expansion.
                    let op = &operands[0];
                    // Keep the exact text bash prints: `$HOME` followed by
                    // the rest of the operand, without normalisation.
                    let shown = match op.text.strip_prefix('~') {
                        Some(rest) if op.tilde && (rest.is_empty() || rest.starts_with('/')) => {
                            format!("{}{rest}", home.display())
                        }
                        _ => op.text.clone(),
                    };
                    assumes.push((i, shown));
                }
                dir = target;
            }
            (Some("pushd" | "popd"), Runs::Shell) => {
                dir = None;
                sticky = false;
                assumes.clear();
            }
            (
                Some("export" | "declare" | "typeset" | "unset" | "local" | "readonly"),
                Runs::Shell,
            ) => {
                let names_context = words[idx + 1..].iter().any(|w| {
                    w.opaque
                        || {
                            let k = assignment_key(&w.text).unwrap_or(&w.text);
                            is_context_key(k) || k == "CDPATH"
                        }
                        || w.text.starts_with('-')
                });
                if names_context {
                    taint_rest = true;
                }
            }
            (Some(p), _) if p == "git" || p.ends_with("/git") => {
                if let Some((call_dir, sub, args, git_opaque)) =
                    parse_git(&words[idx + 1..], dir.as_deref(), home)
                {
                    out.push(GitCall {
                        opaque: opaque
                            || git_opaque
                            || call_dir.is_none()
                            || words.iter().any(|w| w.opaque),
                        dir: call_dir,
                        sub,
                        args,
                        chain_verified: chain_ok,
                        // A `cd` joined to this command only through `&&`
                        // is proven by the command having run.
                        assumes_cd: assumes
                            .iter()
                            .filter(|(j, _)| {
                                lexed.simples[*j..i]
                                    .iter()
                                    .any(|s| s.sep_after != Sep::AndAnd)
                            })
                            .map(|(_, op)| op.clone())
                            .collect(),
                    });
                }
            }
            _ => {}
        }

        if simple.sep_after == Sep::OrOr || simple.sep_after == Sep::Amp {
            seen_or_amp = true;
            sticky = false;
            dir = None;
        } else if simple.sep_after != Sep::AndAnd && !sticky {
            dir = None;
        }
        if dir.is_none() {
            assumes.clear();
        }
    }
    out
}

/// Whether one simple command taints the whole call: its program is
/// unknown (an expansion), or it is a shell builtin or keyword that runs
/// in the calling shell and is neither modelled nor harmless.
fn simple_taints(words: &[Word], idx: usize, runs: Runs) -> bool {
    let Some(w) = words.get(idx) else {
        return false;
    };
    if w.opaque {
        return true;
    }
    let base = w.text.rsplit('/').next().unwrap_or(&w.text);
    let interpreter = matches!(base, "bash" | "sh" | "zsh" | "dash" | "ksh" | "fish");
    // Dispatchers run commands assembled from input this reader cannot see.
    if matches!(base, "xargs" | "parallel" | "sudo" | "doas") {
        return true;
    }
    if runs == Runs::Child {
        // A child cannot change this shell, but a nested interpreter runs
        // commands this reader cannot see (such as another push).
        return interpreter;
    }
    if interpreter {
        return true;
    }
    let prog = w.text.as_str();
    let args = &words[idx + 1..];
    match prog {
        "cd" | "pushd" | "popd" | "export" | "declare" | "typeset" | "unset" | "local"
        | "readonly" => false,
        "set" => !safe_set_args(args),
        "printf" => args.iter().any(|a| a.text.starts_with("-v")),
        // `[[ ]]` evaluates arithmetic, which can assign variables; a test
        // split by `&&`/`||` inside it is not modelled.
        "[[" if args.last().is_none_or(|a| a.text != "]]") => true,
        "[[" => args.iter().any(|a| {
            a.text.contains('=')
                || matches!(
                    a.text.as_str(),
                    "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
                )
        }),
        _ if harmless_builtin(prog) => false,
        _ => is_builtin(prog),
    }
}

type ParsedGit = (Option<PathBuf>, String, Vec<String>, bool);

/// Parse the words after `git`: global options, then the subcommand.
fn parse_git(words: &[Word], dir: Option<&Path>, home: &Path) -> Option<ParsedGit> {
    let mut dir = dir.map(Path::to_path_buf);
    let mut opaque = false;
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let t = w.text.as_str();
        match t {
            "-C" => {
                let d = words.get(i + 1)?;
                dir = dash_c_target(dir.as_deref(), d, home);
                i += 2;
            }
            "-c" => {
                let kv = words.get(i + 1)?;
                if unsafe_config_key(&kv.text) || kv.opaque {
                    opaque = true;
                }
                i += 2;
            }
            "--no-pager" | "-P" | "--no-replace-objects" | "--literal-pathspecs" => i += 1,
            _ if t.starts_with('-') => {
                // `--git-dir`, `--work-tree`, `--namespace`, `--config-env`,
                // and any unknown global option change or obscure context.
                opaque = true;
                i += if matches!(t, "--git-dir" | "--work-tree" | "--namespace") {
                    2
                } else {
                    1
                };
            }
            _ => break,
        }
    }
    let sub = words.get(i)?.text.clone();
    let args = words[i + 1..].iter().map(|w| w.text.clone()).collect();
    Some((dir, sub, args, opaque))
}

fn unsafe_config_key(kv: &str) -> bool {
    let key = kv
        .split_once('=')
        .map_or(kv, |(k, _)| k)
        .to_ascii_lowercase();
    key.starts_with("remote.")
        || key.starts_with("url.")
        || key.starts_with("push.")
        || key.starts_with("branch.")
        || key.starts_with("include")
        || key == "core.worktree"
        || key == "core.bare"
}

impl GitCall {
    fn has(&self, flag: &str) -> bool {
        self.args.iter().any(|a| a == flag)
    }

    fn has_dashdash(&self) -> bool {
        self.has("--")
    }

    fn control_op(&self) -> bool {
        ["--abort", "--quit", "--show-current-patch", "--skip"]
            .iter()
            .any(|f| self.has(f))
    }

    /// `git commit` options that imply `--dry-run`, including `-z` inside
    /// a combined short-option cluster (`-qz`).
    fn commit_dry_run(&self) -> bool {
        self.args.iter().any(|a| {
            if matches!(
                a.as_str(),
                "--dry-run" | "--short" | "--porcelain" | "--long" | "--null"
            ) {
                return true;
            }
            let Some(cluster) = a.strip_prefix('-').filter(|c| !c.starts_with('-')) else {
                return false;
            };
            for c in cluster.chars() {
                match c {
                    'z' => return true,
                    // These take the rest of the cluster as their value.
                    'm' | 'F' | 'C' | 'c' | 't' | 'S' | 'u' => return false,
                    _ => {}
                }
            }
            false
        })
    }

    /// The verb when this invocation is *work* on its root.
    pub fn work_verb(&self) -> Option<MoveVerb> {
        match self.sub.as_str() {
            "commit" if !self.commit_dry_run() => Some(MoveVerb::Commit),
            "rebase" if !self.control_op() => Some(MoveVerb::Rebase),
            "cherry-pick" if !self.control_op() => Some(MoveVerb::CherryPick),
            "merge" if !self.control_op() => Some(MoveVerb::Merge),
            "am" if !self.control_op() => Some(MoveVerb::Am),
            "switch" => Some(MoveVerb::Checkout),
            "checkout"
                if ["-b", "-B", "--detach", "--orphan"]
                    .iter()
                    .any(|f| self.has(f))
                    && !self.has_dashdash() =>
            {
                Some(MoveVerb::Checkout)
            }
            "reset"
                if ["--hard", "--soft", "--keep", "--merge"]
                    .iter()
                    .any(|f| self.has(f))
                    && !self.has_dashdash() =>
            {
                Some(MoveVerb::Reset)
            }
            _ => None,
        }
    }

    /// The verb when this invocation may move HEAD (a move candidate).
    /// Ambiguous `checkout` and `reset` forms are included; the
    /// report-line check proves an actual move.
    pub fn move_verb(&self) -> Option<MoveVerb> {
        self.work_verb().or_else(|| match self.sub.as_str() {
            "checkout" if !self.has_dashdash() && !self.args.is_empty() => Some(MoveVerb::Checkout),
            "reset" if !self.has_dashdash() => Some(MoveVerb::Reset),
            _ => None,
        })
    }

    /// The push details, when this is a `git push`.
    pub fn push(&self) -> Option<PushParts> {
        (self.sub == "push").then(|| push_parts(&self.args))
    }
}

/// Long `git push` options that do not change what is pushed where.
const PUSH_SAFE_LONG: &[&str] = &[
    "--force",
    "--force-if-includes",
    "--no-force-if-includes",
    "--no-verify",
    "--verify",
    "--set-upstream",
    "--quiet",
    "--verbose",
    "--progress",
    "--no-progress",
    "--porcelain",
    "--atomic",
    "--no-atomic",
    "--follow-tags",
    "--no-follow-tags",
    "--thin",
    "--no-thin",
    "--ipv4",
    "--ipv6",
    "--no-signed",
    "--no-recurse-submodules",
];

/// Parse `git push` arguments. Any option this reader does not
/// know marks the push `unsupported` rather than being ignored.
pub fn push_parts(args: &[String]) -> PushParts {
    let mut p = PushParts::default();
    let mut positionals = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--dry-run" => p.dry_run = true,
            "--delete" => p.delete = true,
            "--tags" | "--mirror" | "--all" | "--branches" | "--prune" => p.bulk = true,
            "-o" | "--push-option" => i += 1,
            "--" => {
                positionals.extend(args[i + 1..].iter().cloned());
                break;
            }
            _ if a.starts_with("--force-with-lease")
                || a.starts_with("--push-option=")
                || a.starts_with("--recurse-submodules=")
                || a.starts_with("--signed=") =>
            {
                if a.starts_with("--recurse-submodules=") && a != "--recurse-submodules=no" {
                    p.unsupported = true;
                }
            }
            _ if a.starts_with("--") => {
                if !PUSH_SAFE_LONG.contains(&a) {
                    p.unsupported = true;
                }
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                // Short options, possibly combined (`-nf`, `-uq`).
                for c in a[1..].chars() {
                    match c {
                        'n' => p.dry_run = true,
                        'd' => p.delete = true,
                        'f' | 'u' | 'q' | 'v' | '4' | '6' => {}
                        _ => p.unsupported = true,
                    }
                }
            }
            _ => positionals.push(a.to_string()),
        }
        i += 1;
    }
    let mut it = positionals.into_iter();
    p.remote = it.next();
    for spec in it {
        let spec = spec.strip_prefix('+').unwrap_or(&spec);
        let dst = match spec.split_once(':') {
            Some(("", _)) => {
                p.delete = true;
                continue;
            }
            // A tag pushed onto a branch name is tag activity, not work on
            // the branch.
            Some((src, _)) if src.starts_with("refs/tags/") => continue,
            Some((_, d)) => d,
            None => spec,
        };
        if dst.is_empty() || dst == "HEAD" {
            continue;
        }
        if let Some(b) = dst.strip_prefix("refs/heads/") {
            p.dests.push(b.to_string());
        } else if !dst.starts_with("refs/") {
            p.dests.push(dst.to_string());
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    /// Directories under `/r`, `/A`, `/B`, and `/home/u` exist in tests.
    fn calls(cmd: &str) -> Vec<GitCall> {
        let exists = |p: &Path| {
            ["/r", "/A", "/B", "/home/u"]
                .iter()
                .any(|r| p.starts_with(r))
        };
        git_invocations_with(cmd, Path::new(HOME), &exists)
    }

    fn one(cmd: &str) -> GitCall {
        let mut v = calls(cmd);
        assert_eq!(v.len(), 1, "{cmd}: {v:?}");
        v.remove(0)
    }

    fn dir(c: &GitCall) -> Option<&str> {
        c.dir.as_deref().and_then(Path::to_str)
    }

    #[test]
    fn split_separators_and_redirections() {
        let l = split("cd /a && git push fork x 2>&1 | tail -3; echo ok &");
        let seps: Vec<_> = l.simples.iter().map(|s| s.sep_after).collect();
        assert_eq!(seps, vec![Sep::AndAnd, Sep::Pipe, Sep::Semi, Sep::Amp]);
        let push: Vec<_> = l.simples[1].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(push, vec!["git", "push", "fork", "x"]);
        assert!(!l.tainted);
    }

    #[test]
    fn split_quotes() {
        let l = split(r#"git commit -m "fix: a && b" -m 'c; d'"#);
        assert_eq!(l.simples.len(), 1);
        let w: Vec<_> = l.simples[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(w, vec!["git", "commit", "-m", "fix: a && b", "-m", "c; d"]);
    }

    #[test]
    fn redirect_targets_are_not_arguments() {
        let l = split("git log > /tmp/out.txt 2>/dev/null &>> x");
        let w: Vec<_> = l.simples[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(w, vec!["git", "log"]);
    }

    #[test]
    fn cd_chain_sets_directory() {
        let c = one(
            "cd ~/worktrees/project-pr-topic && git push fork pr-topic 2>&1 | tail -3",
        );
        assert_eq!(
            dir(&c),
            Some("/home/u/worktrees/project-pr-topic")
        );
        assert!(!c.opaque);
        assert!(!c.chain_verified, "a pipe hides the exit status");
        assert_eq!(c.push().unwrap().dests, vec!["pr-topic"]);
    }

    #[test]
    fn unknown_start_directory_is_opaque() {
        let c = one("git commit -m x");
        assert_eq!(c.dir, None);
        assert!(c.opaque);
    }

    #[test]
    fn dash_c_sets_directory() {
        let c = one("git -C /repo/wt commit -am x");
        assert_eq!(dir(&c), Some("/repo/wt"));
        assert!(!c.opaque && c.chain_verified);
        let c = one("cd /repo && git -C wt status");
        assert_eq!(dir(&c), Some("/repo/wt"));
        let c = one("git -C wt status");
        assert!(c.opaque, "relative -C against an unknown directory");
    }

    #[test]
    fn unconditional_cd_persists_when_target_exists() {
        let v = calls("cd /r && git status | head -3; git reset --hard x 2>&1 | tail -1");
        assert_eq!(v.len(), 2);
        assert_eq!(dir(&v[1]), Some("/r"));
        assert!(!v[1].opaque);
        let c = one("cd /missing; git commit -m x");
        assert!(c.opaque, "a missing target may have failed");
        let c = one("x && cd /r; git commit -m x");
        assert!(c.opaque, "a conditional cd does not persist past ;");
        let c = one("echo | cd /r && git commit -m x");
        assert!(c.opaque, "cd in a pipeline runs in a subshell");
        let v = calls("cd /r; cd /missing; git commit -m x");
        assert!(v[0].opaque);
    }

    #[test]
    fn skipped_cd_is_not_trusted() {
        let v = calls("cd /A; test -f use-b && cd /B && git commit -m x");
        assert_eq!(v.len(), 1);
        // `cd /B` ran only if the test succeeded; after `;` the start is
        // unknown, but the `&&` chain proves `cd /B` ran before the commit.
        assert_eq!(dir(&v[0]), Some("/B"));
        let v = calls("cd /A || cd /B && git commit -m x");
        assert!(v[0].opaque, "a cd after || is not proven");
    }

    #[test]
    fn chain_verification() {
        let v = calls("cd /r && git commit -m x && git push f b");
        assert!(v.iter().all(|c| c.chain_verified));
        let v = calls("cd /r && git commit -m x; true");
        assert!(!v[0].chain_verified);
        let v = calls("git -C /r diff --quiet || git -C /r commit -am x");
        assert!(!v[1].chain_verified, "a || before it");
        let v = calls("false && git -C /r push f b; true");
        assert!(!v[0].chain_verified);
    }

    #[test]
    fn taint_rules() {
        assert!(one("cd /r && git push f $(git branch --show-current)").opaque);
        assert!(one("echo \"$(date)\" && git -C /r commit -m x").opaque);
        assert!(one("eval 'x' && git -C /r commit -m x").opaque);
        assert!(one("cd /r && (git commit -m x)").opaque);
        assert!(one("cd /r && git commit -m \"`id`\"").opaque);
        assert!(one("git -C /r/* commit -m x").opaque, "glob");
        assert!(one("git -C /r commit -m $MSG").opaque, "variable");
    }

    #[test]
    fn heredoc_bodies() {
        let quoted = "git -C /r commit -F - <<'EOF'\nfix: $(x)\nEOF\n";
        let c = one(quoted);
        assert!(!c.opaque && c.chain_verified);
        let unquoted = "git -C /r commit -F - <<EOF\nfix: $(x)\nEOF\n";
        assert!(one(unquoted).opaque);
        let plain = "cat <<EOF > f\nhello\nEOF\ngit -C /r commit -m x";
        let c = one(plain);
        assert!(!c.opaque);
    }

    #[test]
    fn cat_heredoc_commit_message_is_literal() {
        let cmd = "cd /r && git commit -m \"$(cat <<'EOF'\nfix: $(not run)\n\nbody `x`\nEOF\n)\" 2>&1 | tail -2";
        let l = split(cmd);
        assert!(!l.tainted);
        let c = one(cmd);
        assert!(!c.opaque);
        assert_eq!(c.args, vec!["-m", "fix: $(not run)\n\nbody `x`"]);
        // An unquoted delimiter expands its body: still tainted.
        assert!(split("git commit -m \"$(cat <<EOF\n$(id)\nEOF\n)\"").tainted);
        // Any other substitution taints.
        assert!(split("git commit -m \"$(cat msg.txt)\"").tainted);
    }

    #[test]
    fn context_environment() {
        assert!(one("GIT_DIR=/o/.git git -C /r commit -m x").opaque);
        assert!(one("HOME=/x git -C /r push f b").opaque);
        assert!(!one("GIT_AUTHOR_NAME=a git -C /r commit -m x").opaque);
        assert!(one("export GIT_DIR=/o && git -C /r commit -m x").opaque);
        assert!(!one("export GIT_AUTHOR_NAME=a && git -C /r commit -m x").opaque);
        assert!(one("env -C /x git -C /r commit -m x").opaque);
        assert!(one("git -c remote.f.url=x -C /r push f b").opaque);
        assert!(!one("git -c user.name=x -C /r commit -m y").opaque);
        assert!(one("git --git-dir=/o -C /r commit -m x").opaque);
    }

    #[test]
    fn verbs() {
        assert_eq!(
            one("git -C /r commit -n -m x").work_verb(),
            Some(MoveVerb::Commit)
        );
        assert_eq!(one("git -C /r commit --dry-run").work_verb(), None);
        assert_eq!(one("git -C /r checkout file.txt").work_verb(), None);
        assert_eq!(
            one("git -C /r checkout file.txt").move_verb(),
            Some(MoveVerb::Checkout)
        );
        assert_eq!(one("git -C /r checkout -- file.txt").move_verb(), None);
        assert_eq!(
            one("git -C /r checkout -b topic").work_verb(),
            Some(MoveVerb::Checkout)
        );
        assert_eq!(
            one("git -C /r switch topic").work_verb(),
            Some(MoveVerb::Checkout)
        );
        assert_eq!(
            one("git -C /r reset --hard HEAD~1").work_verb(),
            Some(MoveVerb::Reset)
        );
        assert_eq!(one("git -C /r reset f.txt").work_verb(), None);
        assert_eq!(
            one("git -C /r reset f.txt").move_verb(),
            Some(MoveVerb::Reset)
        );
        assert_eq!(one("git -C /r rebase --abort").work_verb(), None);
        assert_eq!(one("git -C /r push f b").work_verb(), None);
    }

    #[test]
    fn push_refspecs() {
        let p = |a: &str| push_parts(&a.split_whitespace().map(String::from).collect::<Vec<_>>());
        assert_eq!(p("-u fork topic").dests, vec!["topic"]);
        assert_eq!(p("fork HEAD:topic").dests, vec!["topic"]);
        assert_eq!(p("fork +local:refs/heads/topic").dests, vec!["topic"]);
        assert_eq!(p("--force-with-lease fork a b").dests, vec!["a", "b"]);
        assert!(p("fork HEAD").dests.is_empty());
        assert!(p("fork").dests.is_empty());
        assert!(p("fork v1:refs/tags/v1").dests.is_empty());
        assert!(p("-f fork refs/tags/v1:release").dests.is_empty());
        assert!(p("fork :topic").delete);
        assert!(p("-d fork topic").delete);
        assert!(p("-n fork topic").dry_run);
        assert!(p("--tags fork").bulk);
        assert_eq!(p("-o ci.skip fork t").remote.as_deref(), Some("fork"));
    }

    #[test]
    fn review_b_regressions() {
        // 1. A final background command is not verified.
        assert!(!one("git -C /A commit -m x &").chain_verified);
        // 2. exit/return end analysis.
        assert!(one("exit 0; git -C /A commit -m x").opaque);
        // 3. A cd with a redirection, option, or extra operand is not sticky.
        assert!(one("cd /A; cd /B </missing/f; git commit -m x").opaque);
        assert!(one("cd -P /B; git commit -m x").opaque);
        assert!(one("cd /A /B; git commit -m x").opaque);
        // 4. Taint is found after wrappers.
        assert!(one("cd /A; command eval 'cd /B'; git commit -m x").opaque);
        assert_eq!(
            dir(&one("cd /A; builtin cd /B; git commit -m x")),
            Some("/B")
        );
        // 5. pushd/popd invalidate the directory.
        assert!(one("cd /A; pushd /B; git commit -m x").opaque);
        // 6. Groups and control structures taint.
        assert!(one("cd /A; { cd /B; }; git commit -m x").opaque);
        assert!(one("cd /A; if true; then cd /B; fi; git commit -m x").opaque);
        // 7. CDPATH: a bare relative cd target is unknown.
        assert!(one("cd /A; cd child && git commit -m x").opaque);
        assert!(one("cd /A; CDPATH=/B cd ./child && git commit -m x").opaque);
        assert_eq!(
            dir(&one("cd /A && cd ./child && git commit -m x")),
            Some("/A/child")
        );
        // 8. Logical cd .. and physical -C ..
        assert_eq!(
            dir(&one("cd /A/link && cd .. && git commit -m x")),
            Some("/A")
        );
        assert!(one("git -C /A/link/.. commit -m x").opaque);
        // 9. A quoted tilde is literal.
        assert!(
            one("cd '~/repo' && git commit -m x").opaque,
            "relative literal ~ dir"
        );
        assert_eq!(
            dir(&one("cd ~/repo && git commit -m x")),
            Some("/home/u/repo")
        );
        // 10. An unquoted cat heredoc is word-split: tainted.
        assert!(one("git -C $(cat <<'EOF'\n/A -C /B\nEOF\n) commit -m x").opaque);
        // 11. A tab before the heredoc delimiter.
        let v = calls("cat <<\tEOF\ngit -C /r commit -m fake\nEOF\n");
        assert!(v.is_empty(), "heredoc body is not a command: {v:?}");
        // 12. Commit options that imply --dry-run.
        for o in ["--short", "--porcelain", "--long", "-z", "--dry-run"] {
            assert_eq!(
                one(&format!("git -C /r commit {o}")).work_verb(),
                None,
                "{o}"
            );
        }
        // 13. Combined and unknown push flags.
        let p = |a: &str| push_parts(&a.split_whitespace().map(String::from).collect::<Vec<_>>());
        assert!(p("-nf fork HEAD:topic").dry_run);
        assert!(p("-fd fork topic").delete);
        assert!(p("--some-new-flag fork topic").unsupported);
        assert!(p("-x fork topic").unsupported);
        assert!(!p("-uf --force-with-lease=topic:abc fork topic").unsupported);
    }

    #[test]
    fn review_b2_regressions() {
        // 1. A redirection-only command keeps its place in the chain.
        assert!(!one("false && git -C /A commit -m x; </dev/null").chain_verified);
        // 2. An empty quoted heredoc delimiter: the body is data.
        assert!(calls("cat <<''\ngit -C /A commit -m fake\n\n").is_empty());
        // 3. A child-process wrapper cannot change the shell's directory.
        assert_eq!(dir(&one("cd /A; env cd /B; git commit -m x")), Some("/A"));
        assert_eq!(dir(&one("cd /A; nohup git commit -m x")), Some("/A"));
        // 4. Wrapper options do not hide eval.
        assert!(one("cd /A; command -- eval 'cd /B'; git commit -m x").opaque);
        assert!(one("cd /A; command -p eval x; git commit -m x").opaque);
        // 5. Assignment-only commands that change context.
        assert!(one("HOME=/B; cd ~/repo && git commit -m x").opaque);
        // 6. Variable-writing builtins taint.
        assert!(one("printf -v HOME /B; cd ~/repo && git commit -m x").opaque);
        assert!(one("read HOME </f; cd ~/repo && git commit -m x").opaque);
        // 7. Unsafe `set` options taint; safe ones do not.
        assert!(one("set -n; git -C /A commit -m x").opaque);
        assert!(one("set -P; cd /A/l && cd .. && git commit -m x").opaque);
        assert!(!one("set -euo pipefail; git -C /A commit -m x").opaque);
        // 8. An unknown program word taints.
        assert!(one("cd /A; c=cd; \"$c\" /B; git commit -m x").opaque);
        // 9. A quote inside the tilde prefix makes it literal.
        assert!(one("cd ~\"/repo\" && git commit -m x").opaque);
        assert!(one("cd ~''/repo && git commit -m x").opaque);
        // 10. A sticky cd records what the output must not contradict.
        let c = one("cd /A; cd /B; git commit -m x");
        assert_eq!(dir(&c), Some("/B"));
        assert_eq!(c.assumes_cd, vec!["/A", "/B"]);
        assert!(
            one("cd /A && git commit -m x").assumes_cd.is_empty(),
            "&& proves the cd"
        );
        // 11. A carriage return is part of the word.
        assert!(
            one("cd /A; cd /B\r; git commit -m x").opaque,
            "\"/B\\r\" does not exist"
        );
        // 12. Combined commit flags.
        assert_eq!(one("git -C /A commit -qz").work_verb(), None);
        assert!(
            one("git -C /A commit -qmzap").work_verb().is_some(),
            "z inside -m's value"
        );
    }

    #[test]
    fn review_b3_regressions() {
        // 1. The recorded operand is tilde-expanded.
        let c = one("cd ~/repo; git commit -m x");
        assert_eq!(c.assumes_cd, vec!["/home/u/repo"]);
        // 3. A nested shell under a child wrapper still taints.
        let v = calls("env bash -c 'git -C /B push -n fork HEAD:t'; git -C /A push fork t");
        assert!(v.iter().all(|g| g.opaque));
        // 4. hash can rebind commands.
        assert!(one("hash -p /usr/bin/true git; git -C /A commit -m x").opaque);
        // 5. Mixed quotes in a heredoc delimiter.
        assert!(calls("cat <<\"#'END\"\n#END\ngit -C /A commit -m fake\n#'END\n").is_empty());
        assert!(calls("cat <<E\"N\"D\ngit -C /A commit -m fake\nEND\n").is_empty());
        // 6. A test command does not taint.
        let c = one("[ -d /A ] && git -C /A commit -m x");
        assert!(!c.opaque && c.chain_verified);
        assert!(!one("[[ -d /A ]] && git -C /A commit -m x").opaque);
    }

    #[test]
    fn review_b4_regressions() {
        assert_eq!(one("git -C /A reset --mixed src/f.rs").work_verb(), None);
        assert!(one("[[ HOME=0 -eq 0 ]]; cd ~/repo && git commit -m x").opaque);
        assert!(one("HOME+=/x; cd ~/repo && git commit -m x").opaque);
        assert!(
            calls("env /bin/bash -c 'x'; git -C /A push f t")
                .iter()
                .all(|g| g.opaque)
        );
        assert!(calls("cat <<\"#\\qEND\"\n#qEND\ngit -C /A commit -m fake\n#\\qEND\n").is_empty());
        assert_eq!(
            one("cd ~//repo; git commit -m x").assumes_cd,
            vec!["/home/u//repo"]
        );
    }

    #[test]
    fn review_b5_regressions() {
        assert!(one("[[ -d /tmp && HOME=0 -eq 0 ]]; cd ~/repo && git commit -m x").opaque);
        assert!(one(": $[HOME=0]; cd ~/repo && git commit -m x").opaque);
        // `builtin -p` is invalid: the cd does not run, so the directory
        // must not become /B.
        assert_ne!(
            dir(&one("cd /A; builtin -p cd /B; git commit -m x")),
            Some("/B")
        );
        let v = calls("printf x | xargs git -C /B push; git -C /A push fork topic");
        assert!(v.iter().all(|g| g.opaque));
    }

    #[test]
    fn reflog_messages() {
        assert!(MoveVerb::Commit.matches_reflog("commit (amend): x"));
        assert!(MoveVerb::Checkout.matches_reflog("checkout: moving from a to b"));
        assert!(!MoveVerb::Commit.matches_reflog("rebase (finish): x"));
    }
}
