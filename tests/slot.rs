mod common;

use common::*;
use std::fs;
use std::process::Command;

#[test]
fn verification_slots_are_locked_against_cleanup() {
    let t = T::new();
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: a2", "--", "a.txt"]);
    let slot = t.tmp.join("slots/mux-verify-0");
    assert!(!t.git_ok(&t.repo, &["worktree", "remove", &slot.to_string_lossy()]));
    assert!(slot.join(".git").exists());
}

/// The pre-commit framework stashes every unstaged file in the live tree when it runs there;
/// through git-mux its hooks run in a slot and the live tree keeps everyone's edits.
#[test]
fn pre_commit_framework_runs_in_the_slot() {
    let present = Command::new("pre-commit")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !present && std::env::var_os("GITMUX_TEST_PRE_COMMIT").is_none() {
        eprintln!("pre-commit is not installed; set GITMUX_TEST_PRE_COMMIT=1 to require it");
        return;
    }
    let t = T::new();
    t.write(
        ".pre-commit-config.yaml",
        "repos:\n  - repo: local\n    hooks:\n      - id: fixme\n        name: rewrite fixme\n        entry: perl -pi -e 's/fixme/fixed/g'\n        language: system\n        files: '\\.txt$'\n",
    );
    t.mux(&[
        "commit",
        "-m",
        "area: hooks",
        "--",
        ".pre-commit-config.yaml",
    ]);
    for hook in ["pre-commit", "commit-msg", "pre-push"] {
        fs::remove_file(t.repo.join(".git/hooks").join(hook)).unwrap();
    }
    t.sh(&t.repo, &["pre-commit", "install"]);
    t.write("b.txt", "another session's unsaved edit\n");
    t.write("a.txt", "fixme\n");
    t.mux(&["commit", "-m", "area: a", "--", "a.txt"]);
    assert_eq!(t.head_file("a.txt"), "fixed\n");
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
    assert_eq!(t.sh(&t.repo, &["git", "stash", "list"]), "");
    assert!(!t.git_ok(&t.repo, &["commit", "-qam", "area: plain"]));
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
}
