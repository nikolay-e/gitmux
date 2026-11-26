//! Reads a shell command the way a POSIX shell would split it, closely enough to find the git
//! invocations in it and the directory each one runs in.
use crate::config::expand_user;
use std::path::{Path, PathBuf};

const PUNCT: &[char] = &[';', '&', '|', '(', ')', '\n'];

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Sep,
}

/// Drops here-document bodies: their lines are data, not commands.
fn strip_heredocs(command: &str) -> String {
    let mut out = String::new();
    let mut lines = command.split_inclusive('\n');
    while let Some(line) = lines.next() {
        out.push_str(line);
        let Some(at) = line.find("<<") else { continue };
        let rest = line[at + 2..].trim_start_matches('-').trim_start();
        let word: String = rest
            .trim_start_matches(['\'', '"'])
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if word.is_empty() {
            continue;
        }
        for body in lines.by_ref() {
            if body.trim() == word {
                if body.ends_with('\n') {
                    out.push('\n');
                }
                break;
            }
        }
    }
    out
}

fn lex(s: &str) -> Option<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' => {
                if let Some(w) = cur.take() {
                    toks.push(Tok::Word(w));
                }
            }
            c if PUNCT.contains(&c) => {
                if let Some(w) = cur.take() {
                    toks.push(Tok::Word(w));
                }
                while chars.peek().is_some_and(|n| PUNCT.contains(n)) {
                    chars.next();
                }
                toks.push(Tok::Sep);
            }
            '#' if cur.is_none() => {
                while chars.peek().is_some_and(|n| *n != '\n') {
                    chars.next();
                }
            }
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        ch => w.push(ch),
                    }
                }
            }
            '"' => {
                let w = cur.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            ch @ ('"' | '\\') => w.push(ch),
                            ch => {
                                w.push('\\');
                                w.push(ch);
                            }
                        },
                        ch => w.push(ch),
                    }
                }
            }
            '\\' => cur.get_or_insert_with(String::new).push(chars.next()?),
            ch => cur.get_or_insert_with(String::new).push(ch),
        }
    }
    if let Some(w) = cur {
        toks.push(Tok::Word(w));
    }
    Some(toks)
}

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

fn is_assignment(w: &str) -> bool {
    let Some((name, _)) = w.split_once('=') else {
        return false;
    };
    let mut c = name.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Every git and git-mux invocation in the command, with the directory it runs in (following
/// `cd` and `git -C`; None when it is not known). `git mux …` counts as git-mux. None when the
/// command cannot be parsed.
pub fn invocations(command: &str, cwd: &Path) -> Option<Vec<Invocation>> {
    let toks = lex(&strip_heredocs(command))?;
    let mut out = Vec::new();
    let mut here = cwd.to_path_buf();
    let mut seg: Vec<String> = Vec::new();
    for tok in toks.into_iter().chain([Tok::Sep]) {
        match tok {
            Tok::Word(w) => seg.push(w),
            Tok::Sep => {
                let words: Vec<String> = seg.drain(..).skip_while(|w| is_assignment(w)).collect();
                let Some(first) = words.first() else { continue };
                if first == "cd" && words.len() > 1 {
                    here = here.join(expand_user(&words[1]));
                } else if first == "git" || first.ends_with("/git") {
                    let mut dir = here.clone();
                    if let Some(i) = words.iter().position(|w| w == "-C")
                        && let Some(d) = words.get(i + 1)
                    {
                        dir = here.join(expand_user(d));
                    }
                    let args = words[1..].to_vec();
                    match subcommand(&args) {
                        Some((i, "mux")) => out.push(Invocation {
                            kind: Kind::Mux,
                            dir: Some(dir),
                            args: args[i + 1..].to_vec(),
                        }),
                        _ => out.push(Invocation {
                            kind: Kind::Git,
                            dir: Some(dir),
                            args,
                        }),
                    }
                } else if first.ends_with("git-mux") {
                    out.push(Invocation {
                        kind: Kind::Mux,
                        dir: Some(here.clone()),
                        args: words[1..].to_vec(),
                    });
                }
            }
        }
    }
    Some(out)
}

pub fn argv_invocations(argv: &[String], cwd: &Path) -> Option<Vec<Invocation>> {
    invocations(&argv.join(" "), cwd)
}

fn subcommand(args: &[String]) -> Option<(usize, &str)> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i].as_str(), "-C" | "-c" | "--git-dir" | "--work-tree") {
            2
        } else {
            1
        };
    }
    args.get(i).map(|s| (i, s.as_str()))
}

/// Commit, stash and push are refused by git itself (see install); these rewrite files or the
/// index before any ref moves, so only the tool hook can stop them in time.
pub fn guard_git(args: &[String]) -> Option<&'static str> {
    let (i, sub) = subcommand(args)?;
    let rest = &args[i + 1..];
    let has = |w: &str| rest.iter().any(|a| a == w);
    let broad = rest.iter().any(|a| {
        matches!(
            a.as_str(),
            "." | ":/" | "*" | "-A" | "--all" | "-u" | "--update" | ":"
        )
    });
    match sub {
        "stash" if matches!(rest.first().map(String::as_str), Some("list" | "show")) => None,
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
