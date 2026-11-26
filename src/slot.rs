use crate::repo::Repo;
use crate::run::{Cmd, Result, zsplit};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

pub struct Slot {
    pub dir: PathBuf,
    _lock: File,
}

/// A persistent detached worktree where hooks judge one exact commit, away from the live files.
pub fn slot_at(repo: &Repo, commit: &str, parent: Option<&str>) -> Result<Slot> {
    let mut chosen = None;
    for i in 0..repo.cfg.slots {
        let f = File::create(repo.state.join(format!("slot-{i}.lock")))?;
        if f.try_lock().is_ok() {
            chosen = Some((i, f));
            break;
        }
    }
    let (i, lock) = match chosen {
        Some(c) => c,
        None => {
            eprintln!("git-mux: every verification slot is busy; waiting for slot 0");
            let f = File::create(repo.state.join("slot-0.lock"))?;
            f.lock()?;
            (0, f)
        }
    };
    let dir = repo.cfg.slot_dir.join(format!("mux-verify-{i}"));
    if !dir.join(".git").exists() {
        repo.git().args(["worktree", "prune"]).text()?;
        fs::create_dir_all(&repo.cfg.slot_dir)?;
        repo.git()
            .args(["worktree", "add", "-f", "-f", "--detach"])
            .arg(&dir)
            .arg(repo.head()?)
            .text()?;
    }
    // Locked, so worktree cleanup never deletes a slot and the dependencies built inside it.
    let _ = repo
        .git()
        .args(["worktree", "lock", "--reason", "git-mux verification slot"])
        .arg(&dir)
        .out();
    let git = || Cmd::git(&dir);
    git()
        .args(["checkout", "--detach", "--force", "-q", commit])
        .text()?;
    git().args(["clean", "-fdq"]).text()?;
    // With HEAD at the parent and the index at the commit, the hooks see exactly what a plain
    // `git commit` of this change would show them.
    if let Some(p) = parent {
        git().args(["reset", "--soft", "-q", p]).text()?;
    }
    provision_node_modules(repo, &dir)?;
    Ok(Slot { dir, _lock: lock })
}

/// Hooks resolve their tools from node_modules beside each lockfile. A copy-on-write clone of
/// the live directory costs no blocks and keeps realpath() inside the slot.
fn provision_node_modules(repo: &Repo, slot: &Path) -> Result<()> {
    let locks = repo
        .git()
        .args([
            "ls-files",
            "-z",
            "--",
            "package-lock.json",
            "*/package-lock.json",
        ])
        .raw()?;
    for lock in zsplit(&locks) {
        let parent = Path::new(&lock).parent().unwrap_or(Path::new(""));
        let (live, mine) = (
            repo.root.join(parent).join("node_modules"),
            slot.join(parent).join("node_modules"),
        );
        if !live.is_dir() {
            continue;
        }
        let stamp = ".package-lock.json";
        if mine.is_dir() && fs::read(live.join(stamp)).ok() == fs::read(mine.join(stamp)).ok() {
            continue;
        }
        let _ = fs::remove_dir_all(&mine);
        fs::create_dir_all(mine.parent().unwrap())?;
        let cp = if cfg!(target_os = "macos") {
            Cmd::new("cp").args(["-c", "-R"])
        } else {
            Cmd::new("cp").args(["-R", "--reflink=auto"])
        };
        cp.arg(&live).arg(&mine).text()?;
    }
    Ok(())
}

pub fn run_hook(dir: &Path, name: &str, args: &[&str], stdin: Option<&Path>) -> Result<bool> {
    let mut c = Cmd::git(dir).args(["hook", "run", "--ignore-missing"]);
    if let Some(s) = stdin {
        c = c.arg(format!("--to-stdin={}", s.display()));
    }
    c.arg(name).arg("--").args(args).status()
}

#[allow(dead_code, unused_variables)]
pub fn remove_all(repo: &Repo) -> Result<()> {
    Ok(())
}
