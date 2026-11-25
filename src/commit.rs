use crate::repo::{Entry, Repo, TempPath, read_json, unique, write_atomic};
use crate::run::{Cmd, Result, refuse, zsplit};
use crate::sync::bring_tree;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn build_candidate(
    repo: &Repo,
    base: &str,
    paths: &[String],
) -> Result<(String, BTreeMap<String, Entry>)> {
    let idx = TempPath(unique(&repo.state, "index-"));
    let git = || repo.git().env("GIT_INDEX_FILE", &idx.0);
    git().args(["read-tree", base]).text()?;
    let mut missing = Vec::new();
    for p in paths {
        if fs::symlink_metadata(repo.root.join(p)).is_err()
            && repo.entries(base, std::slice::from_ref(p))?.is_empty()
        {
            missing.push(p.as_str());
        }
    }
    if !missing.is_empty() {
        refuse!("neither on disk nor in HEAD: {}", missing.join(", "));
    }
    git()
        .args(["update-index", "--add", "--remove", "--"])
        .args(paths)
        .text()?;
    let tree = git().arg("write-tree").text()?;
    Ok((tree, repo.index_entries(paths, Some(&idx.0))?))
}

/// Names relative to the checkout; a directory stands for its changed files.
pub fn expand(repo: &Repo, args: &[String], base: Option<&Path>) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    for a in args {
        let Some(p) = repo.rel(a, base) else {
            refuse!("{a} is outside {}", repo.root.display())
        };
        if repo.root.join(&p).is_dir() {
            let sub = repo.dirty(std::slice::from_ref(&p))?;
            if sub.is_empty() {
                refuse!("{p}/ has no changes");
            }
            paths.extend(sub);
        } else {
            paths.push(p);
        }
    }
    let mut seen = BTreeSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    Ok(paths)
}

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

pub struct Verdict {
    pub ok: bool,
    pub fixes: BTreeMap<String, Vec<u8>>,
    pub stray: Vec<String>,
}

pub fn verify(
    repo: &Repo,
    commit: &str,
    parent: &str,
    paths: &[String],
    message: &str,
) -> Result<Verdict> {
    let slot = slot_at(repo, commit, Some(parent))?;
    println!(
        "git-mux: verifying {} in {}",
        &commit[..10],
        slot.dir.display()
    );
    let msg = TempPath(unique(&repo.state, "msg-"));
    fs::write(&msg.0, message)?;
    // The message stage runs first, while the slot is clean: file hooks may rewrite files.
    let msg_ok = run_hook(&slot.dir, "commit-msg", &[&msg.0.to_string_lossy()], None)?;
    let ok = run_hook(&slot.dir, "pre-commit", &[], None)? && msg_ok;
    let git = || Cmd::git(&slot.dir);
    let mut touched = zsplit(&git().args(["diff", "--name-only", "-z"]).raw()?);
    touched.extend(zsplit(
        &git()
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .raw()?,
    ));
    let mut fixes = BTreeMap::new();
    let mut stray = Vec::new();
    for p in touched {
        if !paths.contains(&p) {
            stray.push(p);
        } else if slot.dir.join(&p).is_file() {
            fixes.insert(p.clone(), fs::read(slot.dir.join(&p))?);
        }
    }
    Ok(Verdict { ok, fixes, stray })
}

/// A hook's rewrite reaches the live file only if nobody edited it since the snapshot.
fn apply_fixes(
    repo: &Repo,
    fixes: &BTreeMap<String, Vec<u8>>,
    snapshot: &BTreeMap<String, Entry>,
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut applied, mut clashed) = (Vec::new(), Vec::new());
    for (p, data) in fixes {
        let live = repo.root.join(p);
        let unchanged = match snapshot.get(p) {
            Some((_, oid)) => fs::read(&live).ok() == Some(repo.blob(oid)?),
            None => false,
        };
        if unchanged {
            let mode = fs::metadata(&live)?.permissions().mode();
            write_atomic(&live, data, Some(mode))?;
            applied.push(p.clone());
        } else {
            clashed.push(p.clone());
        }
    }
    Ok((applied, clashed))
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The path prefixes no component claims, and those the candidate's components, their
/// dependencies and dependents read; None when the repository has no components manifest.
fn components_closure(
    repo: &Repo,
    treeish: &str,
    paths: &[String],
) -> Option<(Vec<String>, Vec<String>)> {
    let text = repo
        .git()
        .args(["show", &format!("{treeish}:{}", repo.cfg.components)])
        .bytes()
        .ok()?;
    let manifest: Value = serde_json::from_slice(&text).ok()?;
    let comps: BTreeMap<String, (Vec<String>, Vec<String>)> = manifest["components"]
        .as_array()?
        .iter()
        .filter_map(|c| {
            Some((
                c["name"].as_str()?.to_string(),
                (strs(&c["paths"]), strs(&c["deps"])),
            ))
        })
        .collect();
    let owns: BTreeSet<String> = comps
        .iter()
        .filter(|(_, (ps, _))| {
            paths
                .iter()
                .any(|p| ps.iter().any(|q| p.starts_with(q.as_str())))
        })
        .map(|(n, _)| n.clone())
        .collect();
    let mut users = owns.clone();
    let mut todo: Vec<String> = owns.into_iter().collect();
    while let Some(n) = todo.pop() {
        for (m, (_, deps)) in &comps {
            if deps.contains(&n) && users.insert(m.clone()) {
                todo.push(m.clone());
            }
        }
    }
    let mut reach = users.clone();
    let mut todo: Vec<String> = users.into_iter().collect();
    while let Some(n) = todo.pop() {
        for d in comps.get(&n).map(|c| c.1.clone()).unwrap_or_default() {
            if reach.insert(d.clone()) {
                todo.push(d);
            }
        }
    }
    let covered = comps.values().flat_map(|c| c.0.clone()).collect();
    let mut inputs: Vec<String> = reach
        .iter()
        .filter_map(|n| comps.get(n))
        .flat_map(|c| c.0.clone())
        .collect();
    inputs.extend(strs(&manifest["global"]));
    Some((covered, inputs))
}

/// A passing check is reused across a ref race only when nothing it could read changed: the
/// commits that landed touch neither the candidate's components, their dependencies and
/// dependents, nor any path no component claims (root configuration and tooling).
fn needs_reverify(repo: &Repo, base: &str, head: &str, paths: &[String]) -> Result<bool> {
    let landed = zsplit(
        &repo
            .git()
            .args(["diff", "--name-only", "-z", base, head])
            .raw()?,
    );
    let Some((covered, inputs)) = components_closure(repo, head, paths) else {
        return Ok(!landed.is_empty());
    };
    let starts = |p: &str, set: &[String]| set.iter().any(|q| p.starts_with(q.as_str()));
    Ok(landed
        .iter()
        .any(|p| starts(p, &inputs) || !starts(p, &covered)))
}

pub fn commit(repo: &Repo, message: &str, args: &[String]) -> Result<()> {
    let paths = expand(repo, args, None)?;
    let base = repo.head()?;
    let mut candidate = None;
    for _round in 0..3 {
        let (tree, snapshot) = build_candidate(repo, &base, &paths)?;
        if tree
            == repo
                .git()
                .args(["rev-parse", &format!("{base}^{{tree}}")])
                .text()?
        {
            refuse!("nothing to commit: the named paths match HEAD");
        }
        let c = repo.commit_tree(&tree, &base, message, &[])?;
        let v = verify(repo, &c, &base, &paths, message)?;
        if !v.stray.is_empty() {
            println!(
                "git-mux: hooks also changed files outside the commit, left in the slot: {}",
                v.stray.join(", ")
            );
        }
        if v.ok {
            candidate = Some(c);
            break;
        }
        if v.fixes.is_empty() {
            refuse!("hooks failed; fix the files and run git mux commit again");
        }
        let (applied, clashed) = apply_fixes(repo, &v.fixes, &snapshot)?;
        if !clashed.is_empty() {
            refuse!(
                "hooks rewrote files you edited during the check: {}; run again",
                clashed.join(", ")
            );
        }
        println!(
            "git-mux: applied hook fixes to {}; re-verifying",
            applied.join(", ")
        );
    }
    let Some(candidate) = candidate else {
        refuse!("hooks kept rewriting the files; inspect the slot")
    };
    let landed = publish(repo, &base, &candidate, &paths, message)?;
    self_install(repo, &landed, &paths);
    Ok(())
}

fn publish(
    repo: &Repo,
    base: &str,
    candidate: &str,
    paths: &[String],
    message: &str,
) -> Result<String> {
    let (mut commit, mut parent) = (candidate.to_string(), base.to_string());
    for _attempt in 0..20 {
        let head = repo.head()?;
        if head != parent {
            let r = repo
                .git()
                .args([
                    "merge-tree",
                    "--write-tree",
                    &format!("--merge-base={base}"),
                    &head,
                    candidate,
                ])
                .out()?;
            if !r.ok() {
                refuse!(
                    "another commit on {} conflicts with this one:\n{}",
                    repo.branch,
                    String::from_utf8_lossy(&r.stdout)
                );
            }
            let merged = r.text().lines().next().unwrap_or_default().to_string();
            commit = repo.commit_tree(&merged, &head, message, &[])?;
            if needs_reverify(repo, base, &head, paths)?
                && !verify(repo, &commit, &head, paths, message)?.ok
            {
                refuse!(
                    "the candidate fails on top of {}; run git mux commit again",
                    &head[..10]
                );
            }
            parent = head;
        }
        let journal = Journal::open(repo, "commit", &parent, &commit, paths)?;
        if !repo
            .git()
            .args([
                "update-ref",
                "-m",
                "git-mux",
                &repo.refname,
                &commit,
                &parent,
            ])
            .out()?
            .ok()
        {
            journal.finish();
            continue;
        }
        repo.sync_index(&parent, &commit, paths)?;
        journal.finish();
        println!(
            "git-mux: {} on {} — {}",
            &commit[..10],
            repo.branch,
            message.lines().next().unwrap_or("")
        );
        return Ok(commit);
    }
    refuse!("{} kept moving; run git mux commit again", repo.branch)
}

/// A repository that hosts gitmux's own source (`gitmux.selfInstall`) keeps the running binary at
/// the version that just landed, built from the committed tree rather than the live files.
fn self_install(repo: &Repo, commit: &str, paths: &[String]) {
    let Some(dir) = &repo.cfg.self_install else {
        return;
    };
    if !paths
        .iter()
        .any(|p| p == dir || p.starts_with(&format!("{dir}/")))
    {
        return;
    }
    let result = (|| -> Result<PathBuf> {
        let exe = std::env::current_exe()?;
        let slot = slot_at(repo, commit, None)?;
        let target = repo.state.join("target");
        let built = Cmd::new("cargo")
            .args([
                "build",
                "--release",
                "--locked",
                "--quiet",
                "--manifest-path",
            ])
            .arg(slot.dir.join(dir).join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()?;
        if !built {
            refuse!("cargo build failed");
        }
        let fresh = fs::read(target.join("release").join("git-mux"))?;
        write_atomic(&exe, &fresh, Some(0o755))?;
        Ok(exe)
    })();
    match result {
        Ok(exe) => println!("git-mux: installed the landed git-mux at {}", exe.display()),
        Err(e) => eprintln!("git-mux: the commit landed, but installing its git-mux failed: {e}"),
    }
}

/// A publish moves the branch first and the shared index (and, for sync, the files) after. A
/// process killed or refused in between leaves its journal behind, locked by nobody: the next
/// git-mux run finishes the second half, for the paths that still hold the old state.
pub struct Journal {
    path: PathBuf,
    _file: File,
}

impl Journal {
    pub fn open(
        repo: &Repo,
        kind: &str,
        old: &str,
        new: &str,
        paths: &[String],
    ) -> Result<Journal> {
        let tmp = unique(&repo.state, "journal-");
        let mut f = File::create(&tmp)?;
        f.lock()?;
        f.write_all(
            json!({"kind": kind, "old": old, "new": new, "paths": paths})
                .to_string()
                .as_bytes(),
        )?;
        f.flush()?;
        let path = repo
            .state
            .join(format!("publish.{}.json", std::process::id()));
        fs::rename(&tmp, &path)?;
        Ok(Journal { path, _file: f })
    }

    pub fn finish(self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn heal(repo: &Repo) -> Result<()> {
    for entry in fs::read_dir(&repo.state)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !(name.starts_with("publish.") && name.ends_with(".json")) {
            continue;
        }
        let Ok(f) = File::open(&path) else { continue };
        if f.try_lock().is_err() || !path.exists() {
            continue;
        }
        let Some(j) = read_json(&path) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let (kind, old, new) = (
            j["kind"].as_str().unwrap_or(""),
            j["old"].as_str().unwrap_or(""),
            j["new"].as_str().unwrap_or(""),
        );
        let paths = strs(&j["paths"]);
        let landed = repo
            .git()
            .args(["merge-base", "--is-ancestor", new, &repo.head()?])
            .out()?
            .ok();
        if landed && kind == "commit" {
            repo.sync_index(old, new, &paths)?;
        } else if landed {
            let before = repo.entries(old, &paths)?;
            let mut stale = Vec::new();
            for p in &paths {
                let live = fs::read(repo.root.join(p)).ok();
                let was = before.get(p).map(|(_, oid)| repo.blob(oid)).transpose()?;
                if live == was {
                    stale.push(p.clone());
                }
            }
            bring_tree(repo, new, &stale)?;
        }
        let _ = fs::remove_file(&path);
        eprintln!(
            "git-mux: finished the interrupted {kind} of {}",
            new.get(..10).unwrap_or(new)
        );
    }
    Ok(())
}
