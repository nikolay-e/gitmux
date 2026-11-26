use crate::journal::Journal;
use crate::repo::{Repo, TempPath, unique};
use crate::run::{Result, refuse, zsplit};
use crate::slot::{run_hook, slot_at};
use std::fs;

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
    bring_tree(repo, &tip, &incoming)?;
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

pub fn bring_tree(repo: &Repo, tip: &str, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let present = repo.entries(tip, paths)?;
    let gone: Vec<&String> = paths.iter().filter(|p| !present.contains_key(*p)).collect();
    let _lock = repo.lock("index")?;
    if !present.is_empty() {
        repo.git()
            .args(["checkout", tip, "--"])
            .args(present.keys())
            .text()?;
    }
    if !gone.is_empty() {
        repo.git()
            .args(["rm", "-q", "--ignore-unmatch", "--"])
            .args(gone)
            .text()?;
    }
    Ok(())
}

/// git's own pre-push hook would run in the live tree, where pre-commit stashes every other
/// session's unsaved files while it runs; the same hooks run in a slot on the exact sha.
pub fn push(repo: &Repo) -> Result<()> {
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
        {
            let slot = slot_at(repo, &local, None)?;
            println!(
                "git-mux: pre-push checks for {}..{} in {}",
                &upstream[..10],
                &local[..10],
                slot.dir.display()
            );
            let url = repo.git().args(["remote", "get-url", &remote]).text()?;
            let stdin = TempPath(unique(&repo.state, "pre-push-"));
            fs::write(
                &stdin.0,
                format!("{} {local} {} {upstream}\n", repo.refname, repo.refname),
            )?;
            if !run_hook(&slot.dir, "pre-push", &[&remote, &url], Some(&stdin.0))? {
                refuse!("pre-push checks failed; nothing was pushed");
            }
        }
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
