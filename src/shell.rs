//! Reads a shell command the way a POSIX shell would split it, closely enough to find the git
//! invocations in it and the directory each one runs in. Any word `git` (or ending in `/git`)
//! starts an invocation wherever it stands, so wrappers need no list; `$(…)`, backticks,
//! subshells, `sh -c`, `eval` and heredocs fed to a shell are read the same way. Inline
//! interpreter code (`python -c`) and script files are not parsed.
use crate::run::Cmd;
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];
const SKIPPED: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "time",
    "command", "builtin",
];
const REDIRECTS: &[&str] = &[
    "<<<", "<<-", "<<", "<>", "<&", "<", "&>>", "&>", ">>", ">&", ">|", ">",
];
/// Git never lets an alias shadow one of these, so they need no config lookup.
const BUILTINS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "add",
    "commit",
    "fetch",
    "branch",
    "tag",
    "remote",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
    "describe",
    "config",
    "stash",
    "push",
    "clean",
];

#[derive(Debug, PartialEq)]
pub enum Kind {
    Git,
    Mux,
}

#[derive(Debug)]
pub struct Invocation {
    pub kind: Kind,
    pub dir: Option<PathBuf>,
    pub args: Vec<String>,
}

#[derive(Clone, Default)]
struct Word {
    text: String,
    /// Holds an expansion this reader cannot evaluate (a variable other than HOME, `$(…)`).
    dynamic: bool,
    quoted: bool,
    /// How many leading characters of `text` were typed unquoted (a redirection operator must be).
    lead: usize,
}

impl Word {
    fn plain(text: &str) -> Word {
        Word {
            text: text.into(),
            quoted: true,
            ..Word::default()
        }
    }

    fn push(&mut self, c: char) {
        let tracking = !self.quoted && self.lead == self.text.len();
        self.text.push(c);
        if tracking {
            self.lead = self.text.len();
        }
    }

    fn unknown(&mut self, shown: &str) {
        self.dynamic = true;
        self.text.push_str(shown);
    }
}

#[derive(Clone)]
enum Place {
    Unset,
    At(PathBuf),
    Unknown,
}

/// What a list of commands has done to the shell so far; a subshell works on a copy.
#[derive(Clone)]
struct State {
    dir: Option<PathBuf>,
    git_dir: Place,
    work_tree: Place,
}

#[derive(Clone, Copy, PartialEq)]
enum Term {
    Seq,
    And,
    Or,
    Pipe,
    Background,
    End,
}

enum Redir {
    Here {
        delim: String,
        quoted: bool,
        strip: bool,
    },
    Str(String),
}

struct Pending {
    delim: String,
    quoted: bool,
    strip: bool,
    script: bool,
    st: State,
}

/// The commands of one list, collected until their separator.
struct Frame {
    words: Vec<Word>,
    st: State,
    after_pipe: bool,
    cases: usize,
    /// The next command opens its and-or list.
    first: bool,
    /// A `||` came earlier in this and-or list.
    or_seen: bool,
    /// Compound commands (`if`, loops, `case`, `{`) open around the next command.
    open: usize,
    last: Term,
    /// The state when this and-or list began, which a trailing `&` goes back to.
    start: State,
    /// Where a `cd` of this and-or list leaves the shell once the list or a `||` ends it.
    settle: Option<Option<PathBuf>>,
}

impl Frame {
    fn new(st: State) -> Frame {
        Frame {
            words: Vec::new(),
            start: st.clone(),
            st,
            after_pipe: false,
            cases: 0,
            first: true,
            or_seen: false,
            open: 0,
            last: Term::Seq,
            settle: None,
        }
    }

    /// A `cd` or `pushd` to `to`. Commands joined after it by `&&` run there, since they run only
    /// if it worked; past that the shell is surely there only when the `cd` opened its list
    /// outside any compound command and its target exists, and a `cd` after a `||` may not run.
    fn moved(&mut self, to: Option<PathBuf>, calm: bool) {
        if self.or_seen {
            self.st.dir = None;
            self.settle = Some(None);
            return;
        }
        let sure = calm && self.first && to.as_deref().is_some_and(Path::is_dir);
        self.settle = Some(to.clone().filter(|_| sure));
        self.st.dir = to;
    }

    fn settled(&mut self) {
        if let Some(dir) = self.settle.take() {
            self.st.dir = dir;
        }
    }

    fn ended(&mut self, term: Term) {
        match term {
            Term::Or => {
                self.settled();
                self.or_seen = true;
            }
            Term::Seq | Term::End | Term::Background => {
                if term == Term::Background {
                    self.st = self.start.clone();
                    self.settle = None;
                }
                self.settled();
                self.or_seen = false;
                self.start = self.st.clone();
            }
            Term::And | Term::Pipe => {}
        }
        self.first = matches!(term, Term::Seq | Term::End | Term::Background);
        self.last = term;
    }

    /// Tracks the compound commands a command opens and closes; openers count at the start of a
    /// command (`{` anywhere), closers only there.
    fn track_compound(&mut self, words: &[Word]) {
        self.open += words.iter().filter(|w| w.text == "{").count();
        for w in words {
            match w.text.as_str() {
                "if" | "while" | "until" => self.open += 1,
                "for" | "select" | "case" => {
                    self.open += 1;
                    break;
                }
                "fi" | "done" | "esac" | "}" => self.open = self.open.saturating_sub(1),
                t if SKIPPED.contains(&t) => {}
                _ => break,
            }
        }
    }
}

enum ShellUse {
    Script(String),
    Stdin,
    Plain,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn is_assignment(w: &str) -> bool {
    let Some((name, _)) = w.split_once('=') else {
        return false;
    };
    let mut c = name.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_git(t: &str) -> bool {
    t == "git" || t.ends_with("/git")
}

fn is_mux(t: &str) -> bool {
    t == "git-mux" || t.ends_with("/git-mux")
}

fn base(t: &str) -> &str {
    t.rsplit('/').next().unwrap_or(t)
}

fn join(base: &Option<PathBuf>, text: &str, dynamic: bool) -> Option<PathBuf> {
    if dynamic {
        return None;
    }
    let expanded = match text {
        "~" => home()?,
        t if t.starts_with("~/") => home()?.join(&t[2..]),
        t => PathBuf::from(t),
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        base.as_ref().map(|b| b.join(expanded))
    }
}

fn place(base: &Option<PathBuf>, text: &str, dynamic: bool) -> Place {
    join(base, text, dynamic).map_or(Place::Unknown, Place::At)
}

/// The directory a git command acts in, from the working directory and the three ways to move it.
fn effective_dir(cwd: Option<PathBuf>, git_dir: &Place, work_tree: &Place) -> Option<PathBuf> {
    match (work_tree, git_dir) {
        (Place::At(p), _) | (Place::Unset, Place::At(p)) => Some(p.clone()),
        (Place::Unknown, _) | (Place::Unset, Place::Unknown) => None,
        (Place::Unset, Place::Unset) => cwd,
    }
}

fn redirect_of(w: &Word) -> Option<(&'static str, String)> {
    let lead = &w.text[..w.lead];
    let digits = lead.chars().take_while(char::is_ascii_digit).count();
    let op = REDIRECTS
        .iter()
        .find(|op| lead[digits..].starts_with(**op))?;
    Some((op, w.text[digits + op.len()..].to_string()))
}

/// Separates redirections from the words of a command; their operands are not arguments.
fn split_redirects(words: Vec<Word>) -> (Vec<Word>, Vec<Redir>) {
    let (mut kept, mut redirs) = (Vec::new(), Vec::new());
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
        let Some((op, attached)) = redirect_of(&w) else {
            kept.push(w);
            continue;
        };
        let (operand, quoted) = if attached.is_empty() {
            it.next()
                .map_or((String::new(), false), |n| (n.text, n.quoted))
        } else {
            (attached, w.quoted)
        };
        match op {
            "<<" | "<<-" => redirs.push(Redir::Here {
                delim: operand,
                quoted,
                strip: op == "<<-",
            }),
            "<<<" => redirs.push(Redir::Str(operand)),
            _ => {}
        }
    }
    (kept, redirs)
}

/// How a shell word at `words[i]` gets its commands: `-c` script, standard input, or a file.
fn shell_use(words: &[Word], i: usize) -> ShellUse {
    let (mut c, mut s) = (false, false);
    let mut j = i + 1;
    while let Some(w) = words.get(j) {
        let t = w.text.as_str();
        j += 1;
        if t == "--" {
            break;
        }
        if t == "-" {
            return ShellUse::Stdin;
        }
        if t.starts_with("--") {
            j += usize::from(matches!(t, "--rcfile" | "--init-file"));
        } else if t.len() > 1 && (t.starts_with('-') || t.starts_with('+')) {
            let flags = &t[1..];
            c |= t.starts_with('-') && flags.contains('c');
            s |= t.starts_with('-') && flags.contains('s');
            j += usize::from(flags.ends_with(['o', 'O']));
        } else {
            return if c {
                ShellUse::Script(t.to_string())
            } else if s {
                ShellUse::Stdin
            } else {
                ShellUse::Plain
            };
        }
    }
    match words.get(j) {
        Some(w) if c => ShellUse::Script(w.text.clone()),
        None if !c => ShellUse::Stdin,
        _ => ShellUse::Plain,
    }
}

struct Scan {
    chars: Vec<char>,
    pos: usize,
    pending: Vec<Pending>,
    out: Vec<Invocation>,
    /// A function is defined somewhere in the text, so any later `cd` may run again unseen.
    functions: bool,
}

impl Scan {
    fn new(text: &str) -> Scan {
        Scan {
            chars: text.chars().collect(),
            pos: 0,
            pending: Vec::new(),
            out: Vec::new(),
            functions: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// Reads `text` as a script of its own, in the shell state `st`.
    fn sub(&mut self, text: &str, st: &State, depth: usize) -> Option<()> {
        let mut scan = Scan::new(text);
        scan.list(st.clone(), depth + 1, false)?;
        self.out.append(&mut scan.out);
        Some(())
    }

    fn skip_blanks(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, ' ' | '\t' | '\r') {
                self.pos += 1;
            } else if c == '\\' && self.chars.get(self.pos + 1) == Some(&'\n') {
                self.pos += 2;
            } else {
                break;
            }
        }
    }

    /// Commands up to the end of the text, or past the `)` closing a subshell.
    fn list(&mut self, st: State, depth: usize, paren: bool) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut f = Frame::new(st);
        loop {
            self.skip_blanks();
            let Some(c) = self.peek() else {
                if paren {
                    return None;
                }
                return self.finish(&mut f, Term::End, depth);
            };
            match c {
                '\n' => {
                    self.pos += 1;
                    self.finish(&mut f, Term::Seq, depth)?;
                    self.read_heredocs(depth)?;
                }
                ';' => {
                    self.pos += 1;
                    if self.peek() == Some(';') {
                        self.pos += 1;
                    }
                    if self.peek() == Some('&') {
                        self.pos += 1;
                    }
                    self.finish(&mut f, Term::Seq, depth)?;
                }
                '&' => {
                    self.pos += 1;
                    let term = if self.peek() == Some('&') {
                        self.pos += 1;
                        Term::And
                    } else {
                        Term::Background
                    };
                    self.finish(&mut f, term, depth)?;
                }
                '|' => {
                    self.pos += 1;
                    let term = match self.peek() {
                        Some('|') => Term::Or,
                        _ => Term::Pipe,
                    };
                    if matches!(self.peek(), Some('|' | '&')) {
                        self.pos += 1;
                    }
                    self.finish(&mut f, term, depth)?;
                }
                '(' => {
                    self.pos += 1;
                    let process_substitution = f
                        .words
                        .last()
                        .is_some_and(|w| w.lead == w.text.len() && redirect_of(w).is_some());
                    self.functions |= !f.words.is_empty() && !process_substitution;
                    self.list(f.st.clone(), depth + 1, true)?;
                    if process_substitution {
                        let mut w = Word::default();
                        w.unknown("<(_)");
                        f.words.push(w);
                    }
                }
                ')' => {
                    let first = f.words.first().map(|w| w.text.as_str());
                    let in_case = (f.cases > 0 && first != Some("esac")) || first == Some("case");
                    if !in_case && !paren {
                        return None;
                    }
                    self.pos += 1;
                    if in_case {
                        f.cases += usize::from(first == Some("case"));
                        self.finish(&mut f, Term::Seq, depth)?;
                    } else {
                        return self.finish(&mut f, Term::End, depth);
                    }
                }
                '#' => {
                    while self.peek().is_some_and(|n| n != '\n') {
                        self.pos += 1;
                    }
                }
                _ => {
                    let w = self.word(&f.st, depth)?;
                    f.words.push(w);
                }
            }
        }
    }

    fn word(&mut self, st: &State, depth: usize) -> Option<Word> {
        let mut w = Word::default();
        if self.peek() == Some('~') {
            self.pos += 1;
            match (self.peek(), home()) {
                (None | Some(' ' | '\t' | '\r' | '\n' | '/' | ';' | '&' | '|' | ')'), Some(h)) => {
                    w.text = h.to_string_lossy().into_owned();
                }
                _ => w.unknown("~"),
            }
        }
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '(' | ')' => break,
                '|' | '&' => {
                    let operator = w.lead == w.text.len() && w.text.ends_with(['<', '>']);
                    if !(operator
                        || (c == '&'
                            && w.text.is_empty()
                            && self.chars.get(self.pos + 1) == Some(&'>')))
                    {
                        break;
                    }
                    self.pos += 1;
                    w.push(c);
                }
                '\'' => {
                    self.pos += 1;
                    w.quoted = true;
                    loop {
                        match self.next()? {
                            '\'' => break,
                            ch => w.text.push(ch),
                        }
                    }
                }
                '"' => {
                    self.pos += 1;
                    w.quoted = true;
                    self.double_quoted(st, depth, true, &mut w)?;
                }
                '\\' => {
                    self.pos += 1;
                    match self.next() {
                        Some('\n') | None => {}
                        Some(ch) => {
                            w.quoted = true;
                            w.text.push(ch);
                        }
                    }
                }
                '$' => {
                    self.pos += 1;
                    if self.peek() == Some('\'') {
                        self.pos += 1;
                        w.quoted = true;
                        self.ansi_quoted(&mut w)?;
                    } else {
                        self.dollar(st, depth, &mut w)?;
                    }
                }
                '`' => {
                    self.pos += 1;
                    self.backtick(st, depth, &mut w)?;
                }
                _ => {
                    self.pos += 1;
                    w.push(c);
                }
            }
        }
        Some(w)
    }

    /// Text up to the closing `"` (or the end of a here-document body), with its substitutions
    /// read as commands.
    fn double_quoted(
        &mut self,
        st: &State,
        depth: usize,
        closed: bool,
        w: &mut Word,
    ) -> Option<()> {
        loop {
            match self.next() {
                None if closed => return None,
                None => return Some(()),
                Some('"') if closed => return Some(()),
                Some('\\') => match self.next() {
                    Some(ch @ ('"' | '\\' | '$' | '`')) => w.text.push(ch),
                    Some('\n') => {}
                    Some(ch) => {
                        w.text.push('\\');
                        w.text.push(ch);
                    }
                    None if closed => return None,
                    None => return Some(()),
                },
                Some('$') => self.dollar(st, depth, w)?,
                Some('`') => self.backtick(st, depth, w)?,
                Some(ch) => w.text.push(ch),
            }
        }
    }

    fn ansi_quoted(&mut self, w: &mut Word) -> Option<()> {
        loop {
            match self.next()? {
                '\'' => return Some(()),
                '\\' => w.text.push(match self.next()? {
                    'n' => '\n',
                    't' => '\t',
                    ch => ch,
                }),
                ch => w.text.push(ch),
            }
        }
    }

    fn dollar(&mut self, st: &State, depth: usize, w: &mut Word) -> Option<()> {
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                self.list(st.clone(), depth + 1, true)?;
                w.unknown("$(_)");
            }
            Some('{') => {
                self.pos += 1;
                let name = self.braced(st, depth)?;
                self.variable(&name, w);
            }
            Some(c) if c.is_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(ch) = self.peek().filter(|ch| ch.is_alphanumeric() || *ch == '_') {
                    name.push(ch);
                    self.pos += 1;
                }
                self.variable(&name, w);
            }
            Some(c) if c.is_ascii_digit() || "@*#?$!-".contains(c) => {
                self.pos += 1;
                w.unknown(&format!("${c}"));
            }
            _ => w.text.push('$'),
        }
        Some(())
    }

    fn variable(&self, name: &str, w: &mut Word) {
        match (name, home()) {
            ("HOME", Some(h)) => w.text.push_str(&h.to_string_lossy()),
            _ => w.unknown(&format!("${{{name}}}")),
        }
    }

    fn braced(&mut self, st: &State, depth: usize) -> Option<String> {
        let (mut name, mut open) = (String::new(), 1);
        loop {
            match self.next()? {
                '{' => {
                    open += 1;
                    name.push('{');
                }
                '}' => {
                    open -= 1;
                    if open == 0 {
                        return Some(name);
                    }
                    name.push('}');
                }
                '\\' => {
                    self.next()?;
                }
                '$' if self.peek() == Some('(') => {
                    self.pos += 1;
                    self.list(st.clone(), depth + 1, true)?;
                    name.push_str("$(_)");
                }
                '`' => {
                    let mut w = Word::default();
                    self.backtick(st, depth, &mut w)?;
                    name.push_str(&w.text);
                }
                ch => name.push(ch),
            }
        }
    }

    fn backtick(&mut self, st: &State, depth: usize, w: &mut Word) -> Option<()> {
        let mut inner = String::new();
        loop {
            match self.next()? {
                '`' => break,
                '\\' => match self.next()? {
                    ch @ ('`' | '\\' | '$') => inner.push(ch),
                    ch => {
                        inner.push('\\');
                        inner.push(ch);
                    }
                },
                ch => inner.push(ch),
            }
        }
        self.sub(&inner, st, depth)?;
        w.unknown("$(_)");
        Some(())
    }

    /// Reads the bodies of the here-documents opened on the line just ended.
    fn read_heredocs(&mut self, depth: usize) -> Option<()> {
        for p in std::mem::take(&mut self.pending) {
            let mut body = String::new();
            while self.pos < self.chars.len() {
                let start = self.pos;
                while self.peek().is_some_and(|c| c != '\n') {
                    self.pos += 1;
                }
                let line: String = self.chars[start..self.pos].iter().collect();
                self.pos = (self.pos + 1).min(self.chars.len());
                let text = if p.strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if text.trim_end_matches('\r') == p.delim {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if p.script {
                self.sub(&body, &p.st, depth)?;
            } else if !p.quoted {
                let mut scan = Scan::new(&body);
                scan.double_quoted(&p.st, depth + 1, false, &mut Word::default())?;
                self.out.append(&mut scan.out);
            }
        }
        Some(())
    }

    /// Acts on one finished simple command: finds the git invocations and shells in it and applies
    /// what it does to the directory.
    fn finish(&mut self, f: &mut Frame, term: Term, depth: usize) -> Option<()> {
        let raw = std::mem::take(&mut f.words);
        if raw.is_empty()
            && term == Term::Seq
            && matches!(f.last, Term::And | Term::Or | Term::Pipe)
        {
            return Some(());
        }
        let after_pipe = std::mem::replace(&mut f.after_pipe, term == Term::Pipe);
        let (words, redirs) = split_redirects(raw);
        let head = words
            .iter()
            .position(|w| !(is_assignment(&w.text) || SKIPPED.contains(&w.text.as_str())));
        let first = head.map(|h| words[h].text.as_str());
        f.track_compound(&words);
        self.functions |= words.first().is_some_and(|w| w.text == "function");
        if first == Some("esac") {
            f.cases = f.cases.saturating_sub(1);
        }
        let (mut git_dir, mut work_tree) = (f.st.git_dir.clone(), f.st.work_tree.clone());
        let mut stdin_shell = false;
        for (i, w) in words.iter().enumerate() {
            let t = w.text.as_str();
            if is_assignment(t) {
                assign(&f.st.dir, w, &mut git_dir, &mut work_tree);
            } else if is_git(t) || is_mux(t) {
                self.emit(&words, i, &f.st, (&git_dir, &work_tree), depth)?;
            } else if SHELLS.contains(&base(t)) {
                match shell_use(&words, i) {
                    ShellUse::Script(script) => self.sub(&script, &f.st, depth)?,
                    ShellUse::Stdin => stdin_shell = true,
                    ShellUse::Plain => {}
                }
            } else if t == "eval" && head == Some(i) {
                let script: Vec<&str> = words[i + 1..].iter().map(|w| w.text.as_str()).collect();
                self.sub(&script.join(" "), &f.st, depth)?;
            }
        }
        for redir in redirs {
            match redir {
                Redir::Here {
                    delim,
                    quoted,
                    strip,
                } => self.pending.push(Pending {
                    delim,
                    quoted,
                    strip,
                    script: stdin_shell,
                    st: f.st.clone(),
                }),
                Redir::Str(text) if stdin_shell => self.sub(&text, &f.st, depth)?,
                Redir::Str(_) => {}
            }
        }
        let moves = term != Term::Pipe && !after_pipe;
        let calm = f.open == 0 && !self.functions;
        match first {
            None => {
                for w in &words {
                    assign(&f.st.dir.clone(), w, &mut f.st.git_dir, &mut f.st.work_tree);
                }
            }
            Some("export" | "declare" | "typeset" | "local" | "readonly") => {
                for w in &words[head? + 1..] {
                    assign(&f.st.dir.clone(), w, &mut f.st.git_dir, &mut f.st.work_tree);
                }
            }
            Some(cmd @ ("cd" | "pushd")) if moves => {
                let to = change_dir(&f.st.dir, &words[head? + 1..], cmd == "pushd");
                f.moved(to, calm);
            }
            Some("popd") if moves => f.moved(None, calm),
            _ => {}
        }
        f.ended(term);
        Some(())
    }

    fn emit(
        &mut self,
        words: &[Word],
        i: usize,
        st: &State,
        env: (&Place, &Place),
        depth: usize,
    ) -> Option<()> {
        let args: Vec<String> = words[i + 1..].iter().map(|w| w.text.clone()).collect();
        if is_mux(&words[i].text) {
            let dir = effective_dir(st.dir.clone(), env.0, env.1);
            self.out.push(Invocation {
                kind: Kind::Mux,
                dir,
                args,
            });
            return Some(());
        }
        let (mut cwd, mut git_dir, mut work_tree) = (st.dir.clone(), env.0.clone(), env.1.clone());
        let mut sub_at = words.len();
        let mut j = i + 1;
        while let Some(w) = words.get(j) {
            let t = w.text.as_str();
            if !t.starts_with('-') {
                sub_at = j;
                break;
            }
            let value = words.get(j + 1);
            j += 1;
            match t {
                "-C" => {
                    if let Some(v) = value {
                        cwd = join(&cwd, &v.text, v.dynamic);
                        j += 1;
                    }
                }
                "--git-dir" | "--work-tree" => {
                    if let Some(v) = value {
                        let at = place(&cwd, &v.text, v.dynamic);
                        *if t == "--git-dir" {
                            &mut git_dir
                        } else {
                            &mut work_tree
                        } = at;
                        j += 1;
                    }
                }
                "-c" | "--namespace" | "--super-prefix" | "--config-env" | "--attr-source" => {
                    j += 1;
                }
                _ => {
                    if let Some(v) = t.strip_prefix("--git-dir=") {
                        git_dir = place(&cwd, v, w.dynamic);
                    } else if let Some(v) = t.strip_prefix("--work-tree=") {
                        work_tree = place(&cwd, v, w.dynamic);
                    }
                }
            }
        }
        let dir = effective_dir(cwd, &git_dir, &work_tree);
        let mut args = args;
        let mut at = sub_at - (i + 1);
        for _ in 0..MAX_DEPTH {
            if args.get(at).map(String::as_str) == Some("mux") {
                self.out.push(Invocation {
                    kind: Kind::Mux,
                    dir,
                    args: args.split_off(at + 1),
                });
                return Some(());
            }
            let Some(value) = args.get(at).and_then(|s| alias(dir.as_deref(), s)) else {
                break;
            };
            if let Some(body) = value.strip_prefix('!') {
                let at_dir = State {
                    dir: dir.clone(),
                    git_dir: Place::Unset,
                    work_tree: Place::Unset,
                };
                self.sub(body, &at_dir, depth)?;
                break;
            }
            let expansion = value
                .split_whitespace()
                .map(|s| s.trim_matches(['\'', '"']).to_string());
            args.splice(at..=at, expansion);
            if let Some(next) = args[at..].iter().position(|a| !a.starts_with('-')) {
                at += next;
            }
        }
        self.out.push(Invocation {
            kind: Kind::Git,
            dir,
            args,
        });
        Some(())
    }
}

fn assign(base: &Option<PathBuf>, w: &Word, git_dir: &mut Place, work_tree: &mut Place) {
    let Some((name, value)) = w.text.split_once('=') else {
        return;
    };
    match name {
        "GIT_DIR" => *git_dir = place(base, value, w.dynamic),
        "GIT_WORK_TREE" => *work_tree = place(base, value, w.dynamic),
        _ => {}
    }
}

/// Where `cd`/`pushd` with these arguments leaves the shell; None when that cannot be told.
fn change_dir(from: &Option<PathBuf>, args: &[Word], push: bool) -> Option<PathBuf> {
    let mut operands = args
        .iter()
        .skip_while(|w| w.text.starts_with('-') && w.text.len() > 1 && w.text != "--")
        .filter(|w| w.text != "--");
    match operands.next() {
        None if push => None,
        None => home(),
        Some(w) if w.text == "-" || (push && w.text.starts_with('+')) => None,
        Some(w) => join(from, &w.text, w.dynamic),
    }
}

/// The alias `name` stands for in the repository at `dir`.
fn alias(dir: Option<&Path>, name: &str) -> Option<String> {
    let dir = dir?;
    if name.starts_with('-') || BUILTINS.contains(&name) {
        return None;
    }
    let out = Cmd::git(dir)
        .args(["config", "--get", &format!("alias.{name}")])
        .out()
        .ok()?;
    out.ok().then(|| out.text()).filter(|v| !v.is_empty())
}

/// Every git and git-mux invocation in the command, with the directory it runs in (following
/// `cd` and git's own `-C`, `--git-dir` and `--work-tree`; None when it is not known). `git mux …`
/// counts as git-mux, and a subcommand that is an alias in the repository is replaced by what it
/// stands for. None when the command cannot be parsed.
pub fn invocations(command: &str, cwd: &Path) -> Option<Vec<Invocation>> {
    let mut scan = Scan::new(command);
    scan.list(start(cwd), 0, false)?;
    Some(scan.out)
}

/// A Codex command given as an argv array: one simple command.
pub fn argv_invocations(argv: &[String], cwd: &Path) -> Option<Vec<Invocation>> {
    let mut scan = Scan::new("");
    let mut f = Frame::new(start(cwd));
    f.words = argv.iter().map(|a| Word::plain(a)).collect();
    scan.finish(&mut f, Term::End, 0)?;
    Some(scan.out)
}

fn start(cwd: &Path) -> State {
    State {
        dir: Some(cwd.to_path_buf()),
        git_dir: Place::Unset,
        work_tree: Place::Unset,
    }
}

/// The global options before the subcommand with their values, and where the subcommand is.
fn globals(args: &[String]) -> (Vec<(&str, &str)>, usize) {
    let mut opts = Vec::new();
    let mut i = 0;
    while let Some(a) = args
        .get(i)
        .map(String::as_str)
        .filter(|a| a.starts_with('-'))
    {
        let attached = a.split_once('=').filter(|(name, _)| name.starts_with("--"));
        let takes_value = matches!(
            a,
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
                | "--attr-source"
        );
        i += 1;
        if let Some(pair) = attached {
            opts.push(pair);
        } else if takes_value {
            opts.push((a, args.get(i).map_or("", String::as_str)));
            i += 1;
        } else {
            opts.push((a, ""));
        }
    }
    (opts, i)
}

fn writes_hooks_path(rest: &[String]) -> bool {
    let Some(key) = rest
        .iter()
        .position(|a| a.eq_ignore_ascii_case("core.hooksPath"))
    else {
        return false;
    };
    let any = |words: &[&str]| rest.iter().any(|a| words.contains(&a.as_str()));
    let reads = any(&[
        "--get",
        "--get-all",
        "--get-regexp",
        "--list",
        "-l",
        "get",
        "list",
    ]);
    let writes = any(&[
        "--unset",
        "--unset-all",
        "--add",
        "--replace-all",
        "--edit",
        "-e",
        "set",
        "unset",
    ]);
    writes || (!reads && rest[key + 1..].iter().any(|a| !a.starts_with('-')))
}

/// `git rm` naming everything under a path: `.`, `:/`, a glob, or a directory that exists.
fn rm_is_broad(rest: &[String], dir: Option<&Path>) -> bool {
    let mut pathspecs = false;
    rest.iter().any(|a| {
        if !pathspecs && a == "--" {
            pathspecs = true;
            return false;
        }
        if !pathspecs && a.starts_with('-') {
            return false;
        }
        let exists = |p: &Path| dir.map_or_else(|| p.is_dir(), |d| d.join(p).is_dir());
        matches!(a.as_str(), "." | ":/" | ":")
            || a.contains(['*', '?', '['])
            || exists(Path::new(a))
    })
}

/// Commit, stash and push are refused by git itself (see install), but only after the tool hook
/// has let them start; these rewrite files, the index or the branch before git could object, so
/// only the tool hook can stop them in time. `dir` is where the command runs, when known: a
/// directory named to `git rm` is looked up there, else in this process's own directory.
pub fn guard_in(args: &[String], dir: Option<&Path>) -> Option<&'static str> {
    let (opts, i) = globals(args);
    for (name, value) in opts {
        if matches!(name, "-c" | "--config-env") {
            let key = value.split('=').next().unwrap_or("").to_ascii_lowercase();
            if key.starts_with("alias.") {
                return Some(
                    "an inline alias hides which git command runs; name the command itself",
                );
            }
            if key == "core.hookspath" {
                return Some(
                    "a different hooksPath would run hooks in the shared tree or skip them",
                );
            }
        }
    }
    let sub = args.get(i)?.as_str();
    let rest = &args[i + 1..];
    let has = |w: &str| rest.iter().any(|a| a == w);
    let short = |c: char| {
        rest.iter()
            .any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c))
    };
    let broad = rest.iter().any(|a| {
        matches!(
            a.as_str(),
            "." | ":/" | "*" | "-A" | "--all" | "-u" | "--update" | ":"
        )
    });
    match sub {
        "stash"
            if !matches!(
                rest.first().map(String::as_str),
                Some("list" | "show" | "create")
            ) =>
        {
            Some(
                "stash takes every session's uncommitted edits out of the tree; commit your own paths with git mux, or work in a worktree",
            )
        }
        "commit" => Some(
            "commit only through `git mux commit -m MSG -- PATH…`, which takes your paths alone and runs the hooks in a slot",
        ),
        "push" => Some(
            "push through `git mux push`: it verifies the push in a slot, and --no-verify would skip that",
        ),
        "bisect" => Some("bisect moves HEAD and rewrites the tree; run it in a worktree"),
        "filter-branch" => {
            Some("filter-branch rewrites history and the tree; run it in a clone or worktree")
        }
        "sparse-checkout" if rest.first().map(String::as_str) != Some("list") => {
            Some("sparse-checkout rewrites the shared tree for every session; use a worktree")
        }
        "read-tree" if has("--reset") || short('u') || short('m') => {
            Some("read-tree with -u, -m or --reset overwrites the shared index or tree")
        }
        "checkout-index" if has("--all") || has("--force") || short('a') || short('f') => {
            Some("checkout-index -a or -f overwrites files others are editing; name single files")
        }
        "rm" if rm_is_broad(rest, dir) => {
            Some("rm here names its own files; a directory, glob or `.` removes others' work too")
        }
        "config" if writes_hooks_path(rest) => {
            Some("core.hooksPath decides which hooks run; do not change it in the shared checkout")
        }
        "clean" => Some("untracked files here may be another session's"),
        "pull" => Some("use `git mux sync`: it never touches files someone is editing"),
        "rebase" => Some("rebase rewrites the shared tree; use `git mux sync`, or a worktree"),
        "merge" => Some("merge in a worktree; this checkout stays on its branch for every session"),
        "cherry-pick" => Some("cherry-pick in a worktree; commit here only through git mux"),
        "revert" => Some("revert in a worktree, or restore the file and commit it with git mux"),
        "switch" => Some(
            "this checkout stays on its branch for every session; branch work goes in a worktree",
        ),
        "am" => Some("apply mail in a worktree"),
        "reset" if has("--hard") || has("--merge") || has("--keep") || !has("--") => {
            Some("reset moves the shared index or tree; name paths after `--`, or restore one file")
        }
        "add" if broad => Some("add names its own paths; others' changes live in this tree too"),
        "checkout" | "restore" if broad || (sub == "checkout" && !has("--")) => Some(
            "checkout/restore here names single files after `--`; branch work goes in a worktree",
        ),
        _ => None,
    }
}

/// The paths a `git mux commit …` names.
pub fn commit_paths(args: &[String]) -> Vec<String> {
    if let Some(i) = args.iter().position(|a| a == "--") {
        return args[i + 1..].to_vec();
    }
    let mut paths = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if a == "-m" || a == "--message" {
            skip = true;
        } else if !a.starts_with('-') {
            paths.push(a.clone());
        }
    }
    paths
}

/// The files an `apply_patch` envelope adds, updates, deletes or moves to.
pub fn patch_paths(text: &str) -> Vec<String> {
    let heads = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    text.lines()
        .filter_map(|l| heads.iter().find_map(|h| l.strip_prefix(h)))
        .map(|p| p.trim().to_string())
        .collect()
}
