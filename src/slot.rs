use crate::repo::Repo;
use crate::run::{Cmd, Result, refuse, zsplit};
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

const LOCK_REASON: &str = "git-mux verification slot";

pub struct Slot {
    pub dir: PathBuf,
    /// The checkout's effective hooks dir: what `run_hook` compares the slot's with.
    live_hooks: Option<PathBuf>,
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
        .args(["worktree", "lock", "--reason", LOCK_REASON])
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
    copy_untracked_hooks(repo, &dir)?;
    if let Some(setup) = &repo.cfg.slot_setup {
        let out = Cmd::new("sh")
            .args(["-c", setup])
            .cwd(&dir)
            .env("GITMUX_LIVE", &repo.root)
            .env("GITMUX_SLOT", &dir)
            .env_remove("GIT_LITERAL_PATHSPECS")
            .env_remove("GIT_INDEX_FILE")
            .out()?;
        if !out.ok() {
            refuse!(
                "slotSetup failed ({}): {}{}",
                out.code,
                String::from_utf8_lossy(&out.stdout).trim(),
                out.stderr.trim()
            );
        }
    }
    Ok(Slot {
        dir,
        live_hooks: hooks_dir(&repo.root),
        _lock: lock,
    })
}

fn hooks_dir(dir: &Path) -> Option<PathBuf> {
    let out = Cmd::git(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
        .out()
        .ok()?;
    out.ok().then(|| PathBuf::from(out.text()))
}

/// A hooks directory the repository ignores (husky's generated one) is absent from the slot's
/// checkout, where `git hook run --ignore-missing` would pass silently: its files are copied.
fn copy_untracked_hooks(repo: &Repo, slot: &Path) -> Result<()> {
    let configured = repo
        .git()
        .args(["config", "--get", "core.hooksPath"])
        .out()?
        .text();
    let rel = Path::new(&configured);
    if configured.is_empty()
        || !rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Ok(());
    }
    let listed = repo
        .git()
        .args(["ls-files", "-z", "--others", "--"])
        .arg(rel)
        .raw()?;
    Cmd::git(slot)
        .args(["clean", "-fdxq", "--"])
        .arg(rel)
        .text()?;
    for file in zsplit(&listed) {
        let target = slot.join(&file);
        if target.exists() {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(repo.root.join(&file), &target)?;
    }
    Ok(())
}

fn runs(hook: &Path) -> bool {
    fs::metadata(hook).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Slot {
    pub fn run_hook(&self, name: &str, args: &[&str], stdin: Option<&Path>) -> Result<bool> {
        if let (Some(live), Some(mine)) = (&self.live_hooks, hooks_dir(&self.dir))
            && runs(&live.join(name))
            && !runs(&mine.join(name))
        {
            refuse!(
                "the checkout has an executable {name} hook ({}) that the verification slot lacks ({}); commit it, or ignore it and keep it where the slot copies it from",
                live.join(name).display(),
                mine.join(name).display()
            );
        }
        let mut c = Cmd::git(&self.dir)
            .env_remove("GIT_LITERAL_PATHSPECS")
            .args(["hook", "run", "--ignore-missing"]);
        if let Some(s) = stdin {
            c = c.arg(format!("--to-stdin={}", s.display()));
        }
        c.arg(name).arg("--").args(args).status()
    }
}

/// Unlocks and removes every slot git-mux made, in whatever directory it was created.
pub fn remove_all(repo: &Repo) -> Result<()> {
    let listing = repo
        .git()
        .args(["worktree", "list", "--porcelain", "-z"])
        .raw()?;
    let (mut path, mut ours) = (None, false);
    let mut slots = Vec::new();
    for field in listing.split('\0') {
        if let Some(p) = field.strip_prefix("worktree ") {
            path = Some(p.to_string());
            ours = false;
        } else if field.strip_prefix("locked ") == Some(LOCK_REASON) {
            ours = true;
        } else if field.is_empty() && ours {
            slots.extend(path.take());
            ours = false;
        }
    }
    for slot in &slots {
        let _ = repo.git().args(["worktree", "unlock", slot]).out();
        let _ = repo
            .git()
            .args(["worktree", "remove", "--force", slot])
            .out();
    }
    repo.git().args(["worktree", "prune"]).text()?;
    let _ = fs::remove_dir(&repo.cfg.slot_dir);
    Ok(())
}
