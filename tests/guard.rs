mod common;

use common::*;
use std::fs;

#[test]
fn git_itself_refuses_commit_stash_and_push_in_the_shared_checkout() {
    let t = T::new();
    let head = t.rev(&t.repo, "HEAD");
    t.write("b.txt", "another session's unsaved edit\n");
    t.write("a.txt", "mine\n");
    let ahead = t.sh(
        &t.repo,
        &["git", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "x"],
    );
    let target = t.tmp.join("elsewhere.git").to_string_lossy().into_owned();
    t.sh(&t.tmp, &["git", "init", "-q", "--bare", &target]);
    let refused: [&[&str]; 5] = [
        &["commit", "-qam", "area: x"],
        &["commit", "-qam", "area: x", "--no-verify"],
        &["stash"],
        &["reset", "--soft", ahead.trim()],
        &["push", "-q", &target, "HEAD"],
    ];
    for args in refused {
        assert!(!t.git_ok(&t.repo, args), "{args:?}");
    }
    // `git mux push` runs pre-push in a slot and pushes with --no-verify, so a pre-push hook in
    // the live tree has no legitimate caller: the override does not open it.
    let overridden = cmd("git", &t.tmp)
        .env("GIT_MUX", "1")
        .args(["push", "-q", &target, "HEAD:refs/heads/main"])
        .current_dir(&t.repo)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&overridden.stderr).contains("git mux push"));
    assert!(!t.git_ok(
        &t.tmp.join("elsewhere.git"),
        &["rev-parse", "-q", "--verify", "main"]
    ));
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
    assert_eq!(t.rev(&t.repo, "HEAD"), head);
    assert_eq!(t.sh(&t.repo, &["git", "stash", "list"]), "");
    assert!(t.git_ok(&t.repo, &["gc", "-q"]));

    let remote = t.with_remote();
    t.upstream_commit(&remote, &[("y.txt", Some("y upstream\n"))], "area: y");
    assert!(t.git_ok(&t.repo, &["pull", "-q", "--ff-only", "origin", "main"]));
    assert_eq!(t.read("y.txt"), "y upstream\n");
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");

    let wt = t.tmp.join("wt");
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            &wt.to_string_lossy(),
        ],
    );
    fs::write(wt.join("x.txt"), "branch work\n").unwrap();
    assert!(t.git_ok(&wt, &["commit", "-qam", "area: branch work"]));

    let published = t.rev(&t.repo, "HEAD");
    let ahead = t.sh(
        &t.repo,
        &["git", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "x"],
    );
    assert!(!t.git_ok(&t.repo, &["reset", "--soft", "HEAD~1"]));
    assert!(!t.git_ok(&wt, &["update-ref", "refs/heads/main", ahead.trim()]));
    assert!(!t.git_ok(&wt, &["branch", "-f", "main", "HEAD~1"]));
    assert_eq!(t.rev(&t.repo, "HEAD"), published);
}
