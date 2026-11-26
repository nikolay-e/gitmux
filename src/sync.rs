use crate::journal::Journal;
use crate::repo::{Entry, Repo, TempPath, path_chunks, unique};
use crate::run::{Result, refuse, zsplit};
use crate::slot::slot_at;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Replays local commits onto the fetched upstream inside the object store, and refuses when
/// upstream changes a file someone here is editing or has staged.
pub fn sync(repo: &Repo) -> Result<()> {
    let remote = repo.upstream_remote();
    repo.git()
        .args(["fetch", "-q", &remote, &repo.branch])
        .text()?;
    let upstream = repo
        .git()
        .args([
            "rev-parse",
            &format!("refs/remotes/{remote}/{}", repo.branch),
        ])
        .text()?;
    let local = repo.head()?;
    if repo
        .git()
        .args(["merge-base", "--is-ancestor", &upstream, &local])
        .out()?
        .ok()
    {
        println!(
            "git-mux: {} already contains {remote}/{}",
            repo.branch, repo.branch
        );
        return Ok(());
    }
    let mb = repo.git().args(["merge-base", &local, &upstream]).text()?;
    let range = format!("{mb}..{local}");
    let commits: Vec<String> = repo
        .git()
        .args(["rev-list", "--reverse", "--no-merges", &range])
        .text()?
        .lines()
        .map(String::from)
        .collect();
    if commits.len()
        != repo
            .git()
            .args(["rev-list", &range])
            .text()?
            .lines()
            .count()
    {
        refuse!("local history has merges; integrate it in a worktree");
    }
    let mut tip = upstream.clone();
    for c in &commits {
        let r = repo
            .git()
            .args([
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={c}^"),
                &tip,
                c,
            ])
            .out()?;
        if !r.ok() {
            refuse!(
                "local commit {} conflicts with {remote}/{}:\n{}",
                &c[..10],
                repo.branch,
                String::from_utf8_lossy(&r.stdout)
            );
        }
        let meta = repo
            .git()
            .args(["log", "-1", "--format=%an%x00%ae%x00%ad", c])
            .text()?;
        let mut f = meta.split('\0').map(String::from);
        let env = [
            ("GIT_AUTHOR_NAME", f.next().unwrap_or_default()),
            ("GIT_AUTHOR_EMAIL", f.next().unwrap_or_default()),
            ("GIT_AUTHOR_DATE", f.next().unwrap_or_default()),
        ];
        let message = repo.git().args(["log", "-1", "--format=%B", c]).raw()?;
        tip = repo.commit_tree(
            r.text().lines().next().unwrap_or_default(),
            &tip,
            &message,
            &env,
        )?;
    }
    let incoming = zsplit(
        &repo
            .git()
            .args(["diff", "--name-only", "-z", "--no-renames", &local, &tip])
            .raw()?,
    );
    let busy: Vec<String> = repo.dirty(&incoming)?.into_iter().collect();
    let here = repo.entries(&local, &incoming)?;
    let staged: Vec<String> = repo
        .index_entries(&incoming, None)?
        .into_iter()
        .filter(|(p, e)| here.get(p) != Some(e))
        .map(|(p, _)| p)
        .collect();
    if !busy.is_empty() || !staged.is_empty() {
        refuse!(
            "upstream changes files edited or staged here: {}",
            [busy, staged].concat().join(", ")
        );
    }
    let journal = Journal::open(repo, "sync", &local, &tip, &incoming)?;
    if !repo
        .git()
        .args([
            "update-ref",
            "-m",
            "git-mux sync",
            &repo.refname,
            &tip,
            &local,
        ])
        .out()?
        .ok()
    {
        journal.finish();
        refuse!("{} moved during sync; run it again", repo.branch);
    }
    bring_tree(repo, &local, &tip, &incoming)?;
    journal.finish();
    println!(
        "git-mux: {} at {} ({} local commit(s) on top of {})",
        repo.branch,
        &tip[..10],
        commits.len(),
        &upstream[..10]
    );
    Ok(())
}

/// Moves the live checkout from `old` to `tip` for `paths`, touching only what still holds
/// `old`'s state: an index entry changes while it equals `old`'s, a file is written or removed
/// while its content is `old`'s. A path new in `tip` outside the sparse patterns enters the index
/// skip-worktree and is not written. Plumbing only: git runs no hook in the live tree. Returns the
/// paths left alone.
pub fn bring_tree(repo: &Repo, old: &str, tip: &str, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let (was, will) = (repo.entries(old, paths)?, repo.entries(tip, paths)?);
    let _lock = repo.lock("index")?;
    let cur = repo.index_entries(paths, None)?;
    let skipped = skip_worktree(repo, paths)?;
    let fresh: Vec<String> = paths
        .iter()
        .filter(|p| !was.contains_key(*p) && will.contains_key(*p) && !cur.contains_key(*p))
        .cloned()
        .collect();
    let outside = outside_sparse(repo, &fresh)?;
    let zero = "0".repeat(tip.len());
    let live = live_states(repo, paths)?;
    let mut left = Vec::new();
    let (mut lines, mut keep_skip, mut remove, mut write) =
        (String::new(), Vec::new(), Vec::new(), Vec::new());
    for p in paths {
        let (o, n, c) = (was.get(p), will.get(p), cur.get(p));
        if c != o && c != n {
            left.push(p.clone());
            continue;
        }
        if c == o && o != n {
            let (mode, oid) = n.cloned().unwrap_or(("0".into(), zero.clone()));
            lines.push_str(&format!("{mode} {oid}\t{p}\0"));
        }
        if skipped.contains(p) || outside.contains(p) {
            if n.is_some() {
                keep_skip.push(p.clone());
            }
            continue;
        }
        match content(repo, &live[p], o, n) {
            Content::Old if n.is_some() => write.push(p.clone()),
            Content::Old => remove.push(p.clone()),
            Content::Other => left.push(p.clone()),
            Content::New => {}
        }
    }
    let r = repo.update_index(&["-z", "--index-info"], lines.into_bytes())?;
    if !r.ok() {
        refuse!("updating the shared index failed: {}", r.stderr.trim());
    }
    if !keep_skip.is_empty() {
        let input = keep_skip.join("\0") + "\0";
        repo.update_index(&["-z", "--skip-worktree", "--stdin"], input.into_bytes())?;
    }
    for p in &remove {
        if fs::remove_file(repo.root.join(p)).is_ok() {
            let mut dir = repo.root.join(p);
            while dir.pop() && dir != repo.root && fs::remove_dir(&dir).is_ok() {}
        } else {
            left.push(p.clone());
        }
    }
    write.retain(|p| {
        let blocked = Path::new(p)
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .any(|a| fs::symlink_metadata(repo.root.join(a)).is_ok_and(|m| !m.is_dir()));
        if blocked {
            left.push(p.clone());
        }
        !blocked
    });
    if !write.is_empty() {
        let input = write.join("\0") + "\0";
        repo.git()
            .args(["checkout-index", "-f", "-z", "--stdin"])
            .input(input)
            .text()?;
    }
    let _ = repo.git().args(["update-index", "-q", "--refresh"]).out();
    left.sort();
    if !left.is_empty() {
        eprintln!(
            "git-mux: left alone, changed or staged here since the sync began: {}",
            left.join(", ")
        );
    }
    Ok(left)
}

fn skip_worktree(repo: &Repo, paths: &[String]) -> Result<BTreeSet<String>> {
    let mut skipped = BTreeSet::new();
    for chunk in path_chunks(paths) {
        let out = repo
            .git()
            .args(["ls-files", "-t", "-z", "--"])
            .args(chunk)
            .raw()?;
        skipped.extend(
            zsplit(&out)
                .iter()
                .filter_map(|l| l.strip_prefix("S ").map(String::from)),
        );
    }
    Ok(skipped)
}

/// Of `paths`, those that a sparse checkout does not materialize.
fn outside_sparse(repo: &Repo, paths: &[String]) -> Result<BTreeSet<String>> {
    let sparse = repo
        .git()
        .args(["config", "--bool", "core.sparseCheckout"])
        .out()?
        .text()
        == "true";
    if !sparse || paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let inside = repo
        .git()
        .args(["sparse-checkout", "check-rules", "-z"])
        .input(paths.join("\0") + "\0")
        .raw()?;
    let inside: BTreeSet<String> = zsplit(&inside).into_iter().collect();
    Ok(paths
        .iter()
        .filter(|p| !inside.contains(*p))
        .cloned()
        .collect())
}

/// What a live path holds.
enum Live {
    Absent,
    Link(Vec<u8>),
    File(String),
    Dir,
}

enum Content {
    Old,
    New,
    Other,
}

/// Classifies the live files of `paths`; regular files are hashed as git would store them.
fn live_states(repo: &Repo, paths: &[String]) -> Result<BTreeMap<String, Live>> {
    let mut states = BTreeMap::new();
    let (mut batch, mut odd) = (Vec::new(), Vec::new());
    for p in paths {
        let full = repo.root.join(p);
        let state = match fs::symlink_metadata(&full) {
            Err(_) => Live::Absent,
            Ok(m) if m.file_type().is_symlink() => {
                Live::Link(fs::read_link(&full)?.into_os_string().into_encoded_bytes())
            }
            Ok(m) if m.is_file() => {
                if p.contains('\n') {
                    &mut odd
                } else {
                    &mut batch
                }
                .push(p.clone());
                continue;
            }
            Ok(_) => Live::Dir,
        };
        states.insert(p.clone(), state);
    }
    if !batch.is_empty() {
        let hashes = repo
            .git()
            .args(["hash-object", "--stdin-paths"])
            .input(batch.join("\n") + "\n")
            .text()?;
        for (p, h) in batch.into_iter().zip(hashes.lines()) {
            states.insert(p, Live::File(h.to_string()));
        }
    }
    for p in odd {
        let h = repo.git().args(["hash-object", "--", &p]).text()?;
        states.insert(p, Live::File(h));
    }
    Ok(states)
}

fn content(repo: &Repo, live: &Live, old: Option<&Entry>, new: Option<&Entry>) -> Content {
    let is = |e: Option<&Entry>| match (e, live) {
        (None, Live::Absent) => true,
        (Some((mode, oid)), Live::Link(target)) => {
            mode == "120000" && repo.blob(oid).is_ok_and(|b| &b == target)
        }
        (Some((mode, oid)), Live::File(hash)) => mode != "120000" && oid == hash,
        _ => false,
    };
    if is(old) {
        Content::Old
    } else if is(new) {
        Content::New
    } else {
        Content::Other
    }
}

/// git's own pre-push hook would run in the live tree, where pre-commit stashes every other
/// session's unsaved files while it runs; the same hooks run in a slot on the exact sha.
pub fn push(repo: &Repo, refs: &[String]) -> Result<()> {
    if !refs.is_empty() {
        return push_refs(repo, refs);
    }
    let remote = repo.upstream_remote();
    for _attempt in 0..3 {
        sync(repo)?;
        let local = repo.head()?;
        let upstream = repo
            .git()
            .args([
                "rev-parse",
                &format!("refs/remotes/{remote}/{}", repo.branch),
            ])
            .text()?;
        if local == upstream {
            println!(
                "git-mux: {remote}/{} is already at {}",
                repo.branch,
                &local[..10]
            );
            return Ok(());
        }
        pre_push(
            repo,
            &remote,
            &local,
            &format!("{} {local} {} {upstream}", repo.refname, repo.refname),
        )?;
        let r = repo
            .git()
            .args([
                "push",
                "--no-verify",
                &remote,
                &format!("{local}:{}", repo.refname),
            ])
            .out()?;
        print!("{}{}", String::from_utf8_lossy(&r.stdout), r.stderr);
        if r.ok() {
            return Ok(());
        }
        if !r.stderr.contains("non-fast-forward") && !r.stderr.contains("fetch first") {
            refuse!("push failed");
        }
    }
    refuse!(
        "{remote}/{} kept moving; run git mux push again",
        repo.branch
    )
}

/// Runs pre-push in a slot on `commit`, fed the one line git would give it.
fn pre_push(repo: &Repo, remote: &str, commit: &str, line: &str) -> Result<()> {
    let slot = slot_at(repo, commit, None)?;
    println!(
        "git-mux: pre-push checks for {} in {}",
        &commit[..10],
        slot.dir.display()
    );
    let url = repo.git().args(["remote", "get-url", remote]).text()?;
    let stdin = TempPath(unique(&repo.state, "pre-push-"));
    fs::write(&stdin.0, format!("{line}\n"))?;
    if !slot.run_hook("pre-push", &[remote, &url], Some(&stdin.0))? {
        refuse!("pre-push checks failed; nothing was pushed");
    }
    Ok(())
}

/// Each named ref (branch, tag or `src:dst`) is checked and pushed on its own; the first
/// refusal stops.
fn push_refs(repo: &Repo, refs: &[String]) -> Result<()> {
    let remote = repo.upstream_remote();
    for spec in refs {
        let (src, dst) = spec.split_once(':').unwrap_or((spec, spec));
        if spec.starts_with('+') || src.is_empty() {
            refuse!("{spec}: forced pushes and deletions are not supported");
        }
        let local_ref = repo
            .git()
            .args(["rev-parse", "--symbolic-full-name", src])
            .text()?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        if !local_ref.starts_with("refs/") {
            refuse!("{src} is not a branch or a tag");
        }
        let oid = repo
            .git()
            .args(["rev-parse", "--verify", &local_ref])
            .text()?;
        let commit = repo
            .git()
            .args(["rev-parse", "--verify", &format!("{local_ref}^{{commit}}")])
            .text()?;
        let remote_ref = match dst {
            d if d.starts_with("refs/") => d.to_string(),
            d if local_ref.starts_with("refs/tags/") => format!("refs/tags/{d}"),
            d => format!("refs/heads/{d}"),
        };
        let listed = repo
            .git()
            .args(["ls-remote", &remote, &remote_ref])
            .text()?;
        let remote_oid = listed
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .find(|(_, name)| *name == remote_ref)
            .map_or("0".repeat(oid.len()), |(id, _)| id.to_string());
        pre_push(
            repo,
            &remote,
            &commit,
            &format!("{local_ref} {oid} {remote_ref} {remote_oid}"),
        )?;
        let r = repo
            .git()
            .args(["push", "--no-verify", &remote])
            .arg(format!("{local_ref}:{remote_ref}"))
            .out()?;
        print!("{}{}", String::from_utf8_lossy(&r.stdout), r.stderr);
        if !r.ok() {
            refuse!("push of {spec} failed");
        }
    }
    Ok(())
}

pub fn status(repo: &Repo) -> Result<()> {
    let held = repo.leases()?;
    for (p, v) in &held {
        let gone = if crate::lease::alive(v) {
            ""
        } else {
            " (gone)"
        };
        println!("{p}\t{}{gone}", v["session"].as_str().unwrap_or("?"));
    }
    let loose: Vec<String> = repo
        .dirty(&[".".into()])?
        .into_iter()
        .filter(|p| !held.contains_key(p))
        .collect();
    if !loose.is_empty() {
        println!("-- changed, unclaimed ({}):", loose.len());
        println!("{}", loose.join("\n"));
    }
    Ok(())
}
