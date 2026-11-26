use crate::journal::Journal;
use crate::lease::{Owner, refuse_foreign};
use crate::repo::{Entry, Repo, TempPath, strs, unique, write_atomic};
use crate::run::{Cmd, Result, refuse, zsplit};
use crate::slot::slot_at;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
    let absent: Vec<String> = paths
        .iter()
        .filter(|p| fs::symlink_metadata(repo.root.join(p)).is_err())
        .cloned()
        .collect();
    let known = repo.entries(base, &absent)?;
    let missing: Vec<&str> = absent
        .iter()
        .filter(|p| !known.contains_key(*p))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        refuse!("neither on disk nor in HEAD: {}", missing.join(", "));
    }
    git()
        .args(["update-index", "--add", "--remove", "-z", "--stdin"])
        .input(paths.iter().map(|p| format!("{p}\0")).collect::<String>())
        .text()?;
    let tree = git().arg("write-tree").text()?;
    Ok((tree, repo.index_entries(paths, Some(&idx.0))?))
}

/// Names relative to the checkout; a directory, deleted or not, stands for its changed files.
pub fn expand(repo: &Repo, args: &[String], base: Option<&Path>) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    for a in args {
        let Some(p) = repo.rel(a, base) else {
            refuse!("{a} is outside {}", repo.root.display())
        };
        let kind = fs::symlink_metadata(repo.root.join(&p)).ok();
        if kind.as_ref().is_some_and(|m| !m.is_dir()) {
            paths.push(p);
            continue;
        }
        let sub = repo.dirty(std::slice::from_ref(&p))?;
        if !sub.is_empty() {
            paths.extend(sub);
        } else if kind.is_some() {
            refuse!("{p}/ has no changes");
        } else if outside_sparse(repo, &p)? {
            refuse!("{p} is outside the sparse checkout, not deleted");
        } else {
            paths.push(p);
        }
    }
    let mut seen = BTreeSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    Ok(paths)
}

/// A path missing from disk whose index entry is skip-worktree was never checked out.
fn outside_sparse(repo: &Repo, path: &str) -> Result<bool> {
    let tags = repo
        .git()
        .args(["ls-files", "-t", "-z", "--", path])
        .raw()?;
    Ok(zsplit(&tags).iter().any(|e| e.starts_with("S ")))
}

pub struct Fix {
    data: Vec<u8>,
    mode: u32,
}

pub struct Verdict {
    pub ok: bool,
    pub fixes: BTreeMap<String, Fix>,
    pub stray: Vec<String>,
    /// Named paths a hook deleted or replaced by something that is not a regular file.
    pub broken: Vec<String>,
    /// The message after prepare-commit-msg and commit-msg, as git would cut it.
    pub message: String,
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
    let file = msg.0.to_string_lossy();
    let run = |name: &str, args: &[&str]| slot.run_hook(name, args, None);
    let ok = run("pre-commit", &[])?
        && run("prepare-commit-msg", &[&file, "message"])?
        && run("commit-msg", &[&file])?;
    let git = || Cmd::git(&slot.dir);
    let mut message = message.to_string();
    if ok {
        message = git().arg("stripspace").input(fs::read(&msg.0)?).raw()?;
        if message.is_empty() {
            refuse!("the commit message is empty after the hooks ran");
        }
    }
    let diff = |extra: &[&str]| {
        git()
            .args(["diff", "--name-only", "-z", "--no-renames"])
            .args(extra)
            .arg(commit)
            .raw()
    };
    let mut touched: BTreeSet<String> = zsplit(&diff(&[])?).into_iter().collect();
    touched.extend(zsplit(&diff(&["--cached"])?));
    touched.extend(zsplit(
        &git()
            .args(["ls-files", "-z", "--others", "--exclude-standard"])
            .raw()?,
    ));
    let (mut fixes, mut stray, mut broken) = (BTreeMap::new(), Vec::new(), Vec::new());
    for p in touched {
        if !paths.contains(&p) {
            stray.push(p);
            continue;
        }
        let file = slot.dir.join(&p);
        match fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => {
                let mode = m.permissions().mode() & 0o7777;
                fixes.insert(
                    p,
                    Fix {
                        data: fs::read(&file)?,
                        mode,
                    },
                );
            }
            _ => broken.push(p),
        }
    }
    Ok(Verdict {
        ok,
        fixes,
        stray,
        broken,
        message,
    })
}

/// What `git add` would store for the live file, or None when it is missing.
fn live_blob(repo: &Repo, path: &str) -> Option<String> {
    let out = repo
        .git()
        .arg("hash-object")
        .arg(format!("--path={path}"))
        .arg("--")
        .arg(repo.root.join(path))
        .out()
        .ok()?;
    out.ok().then(|| out.text())
}

/// A hook's rewrite reaches the live file only while the file still holds what was verified.
fn apply_fixes(
    repo: &Repo,
    fixes: &BTreeMap<String, Fix>,
    snapshot: &BTreeMap<String, Entry>,
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut applied, mut clashed) = (Vec::new(), Vec::new());
    for (p, fix) in fixes {
        let live = repo.root.join(p);
        let unchanged = match (snapshot.get(p), fs::symlink_metadata(&live)) {
            (Some((mode, oid)), Ok(m)) => {
                m.is_file() && mode != "120000" && live_blob(repo, p).as_ref() == Some(oid)
            }
            (None, Err(e)) => e.kind() == std::io::ErrorKind::NotFound,
            _ => false,
        };
        if unchanged {
            if let Some(dir) = live.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(&live, &fix.data, Some(fix.mode))?;
            applied.push(p.clone());
        } else {
            clashed.push(p.clone());
        }
    }
    Ok((applied, clashed))
}

/// `owner` claims `path` when it names the path itself or a directory above it, never a longer
/// name that merely starts with the same text.
fn within(path: &str, owner: &str) -> bool {
    let owner = owner.trim_end_matches('/');
    owner.is_empty()
        || owner == "."
        || path == owner
        || path
            .strip_prefix(owner)
            .is_some_and(|rest| rest.starts_with('/'))
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
        .filter(|(_, (ps, _))| paths.iter().any(|p| ps.iter().any(|q| within(p, q))))
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
    let starts = |p: &str, set: &[String]| set.iter().any(|q| within(p, q));
    Ok(landed
        .iter()
        .any(|p| starts(p, &inputs) || !starts(p, &covered)))
}

pub fn commit(repo: &Repo, message: &str, args: &[String], take: bool) -> Result<()> {
    let paths = expand(repo, args, None)?;
    refuse_foreign(repo, &paths, &Owner::of_process(), take)?;
    let base = repo.head()?;
    let mut candidate = None;
    let mut message = message.to_string();
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
        let c = repo.commit_tree(&tree, &base, &message, &[])?;
        let v = verify(repo, &c, &base, &paths, &message)?;
        if !v.stray.is_empty() {
            println!(
                "git-mux: hooks also changed files outside the commit, left in the slot: {}",
                v.stray.join(", ")
            );
        }
        if !v.broken.is_empty() {
            refuse!(
                "hooks deleted or replaced files named in the commit: {}",
                v.broken.join(", ")
            );
        }
        if v.ok && v.fixes.is_empty() {
            candidate = Some(if v.message == message {
                c
            } else {
                repo.commit_tree(&tree, &base, &v.message, &[])?
            });
            message = v.message;
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
    let landed = publish(repo, &base, &candidate, &paths, &message)?;
    self_install(repo, &landed, &paths);
    Ok(())
}

/// A passing check on the merged commit: no failure, and nothing a hook would still rewrite.
fn verifies(
    repo: &Repo,
    commit: &str,
    parent: &str,
    paths: &[String],
    message: &str,
) -> Result<bool> {
    let v = verify(repo, commit, parent, paths, message)?;
    Ok(v.ok && v.fixes.is_empty() && v.broken.is_empty())
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
                && !verifies(repo, &commit, &head, paths, message)?
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
