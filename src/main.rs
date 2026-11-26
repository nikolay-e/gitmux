//! Several agents edit one checkout at once, on one branch, and see each other's saved files
//! immediately. Plain `git commit` cannot serve that: pre-commit stashes every unstaged file in
//! the tree (another session's work vanishes mid-edit), the shared index sweeps in whatever
//! someone else staged, and whole-project hooks judge a mix of everyone's half-saved files.
//!
//! git-mux commits only the paths it is given. It snapshots them into a private index built from
//! HEAD, runs the repository's own hooks on that exact candidate in a persistent verification
//! worktree, then publishes with a compare-and-swap ref update. If another agent moved the branch
//! meanwhile, the candidate is merged onto the new tip inside the object store (git merge-tree),
//! never through the working tree or the shared index.
mod commit;
mod config;
mod guard;
mod hook;
mod install;
mod journal;
mod lease;
mod repo;
mod run;
mod shell;
mod slot;
mod sync;

use run::{Refused, Result};
use std::process::ExitCode;

const USAGE: &str = "usage: git mux commit -m MSG [--take] [--] PATH...
       git mux sync      replay local commits onto the upstream, sparing files in use
       git mux push      sync, run the pre-push hooks in a slot, push that exact commit
       git mux status    who holds which uncommitted file
       git mux install   guard this checkout and wire the Claude Code / Codex hooks
       git mux hook      (called by the agents around every tool call)";

fn message_and_paths(args: &[String]) -> Result<(String, Vec<String>, bool)> {
    let mut messages = Vec::new();
    let mut paths = Vec::new();
    let mut take = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => paths.extend(it.by_ref().cloned()),
            "-m" | "--message" => messages.push(
                it.next()
                    .ok_or_else(|| Refused(format!("{a} needs a message")))?
                    .clone(),
            ),
            "--take" => take = true,
            m if m.starts_with("--message=") => messages.push(m["--message=".len()..].to_string()),
            m if m.starts_with("-m") => messages.push(m[2..].to_string()),
            m if m.starts_with('-') => return Err(Refused(format!("unknown option {m}\n{USAGE}"))),
            p => paths.push(p.to_string()),
        }
    }
    if messages.is_empty() || paths.is_empty() {
        return Err(Refused(USAGE.into()));
    }
    let message = messages.join("\n\n");
    Ok((
        if message.ends_with('\n') {
            message
        } else {
            message + "\n"
        },
        paths,
        take,
    ))
}

fn run(args: &[String]) -> Result<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("");
    if !matches!(cmd, "commit" | "sync" | "push" | "status" | "install") {
        return Err(Refused(USAGE.into()));
    }
    let repo = repo::Repo::open(&std::env::current_dir()?)?;
    let notes = install::ensure(&repo)?;
    if matches!(cmd, "install" | "status") {
        for note in notes {
            eprintln!("{note}");
        }
    }
    journal::heal(&repo)?;
    match cmd {
        "commit" => {
            let (message, paths, take) = message_and_paths(&args[1..])?;
            commit::commit(&repo, &message, &paths, take)
        }
        "sync" => sync::sync(&repo),
        "push" => sync::push(&repo, &args[1..]),
        "status" => sync::status(&repo),
        _ => install::install(&repo),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") => {
            // A crash must deny the tool call rather than let it through unguarded.
            std::panic::set_hook(Box::new(|info| {
                eprintln!("git-mux: the hook crashed, so the call is refused: {info}");
                std::process::exit(2);
            }));
            return hook::main();
        }
        Some("--version" | "-V") => {
            println!("git-mux {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    // Our own ref moves pass the reference-transaction guard; nothing else here is threaded yet.
    unsafe { std::env::set_var("GIT_MUX", "1") };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git-mux: {e}");
            ExitCode::FAILURE
        }
    }
}
