mod common;

use common::*;
use std::process::Stdio;

#[test]
fn commit_takes_only_named_paths_and_keeps_foreign_work() {
    let t = T::new();
    t.write("b.txt", "BROKEN half-saved edit of another session\n");
    t.write("c.txt", "staged by another session\n");
    t.sh(&t.repo, &["git", "add", "c.txt"]);
    t.write("a.txt", "a2\n");

    t.mux(&["commit", "-m", "area: change a", "--", "a.txt"]);

    assert_eq!(t.head_file("a.txt"), "a2\n");
    assert_eq!(t.head_file("b.txt"), "b\n");
    assert!(t.read("b.txt").starts_with("BROKEN"));
    assert_eq!(
        t.sh(&t.repo, &["git", "diff", "--cached", "--name-only"]),
        "c.txt\n"
    );
    assert_eq!(t.sh(&t.repo, &["git", "diff", "--name-only"]), "b.txt\n");
    assert_eq!(t.sh(&t.repo, &["git", "stash", "list"]), "");
}

#[test]
fn failing_hook_and_message_publish_nothing() {
    let t = T::new();
    let before = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "BAD\n");
    assert!(
        !t.run(&["commit", "-m", "area: bad", "--", "a.txt"], &[])
            .status
            .success()
    );
    t.write("a.txt", "good\n");
    assert!(
        !t.run(&["commit", "-m", "no area", "--", "a.txt"], &[])
            .status
            .success()
    );
    assert_eq!(t.rev(&t.repo, "HEAD"), before);
}

#[test]
fn formatter_output_reaches_the_live_file() {
    let t = T::new();
    t.write("a.txt", "fixme\n");
    t.mux(&["commit", "-m", "area: fix", "a.txt"]);
    assert_eq!(t.head_file("a.txt"), "fixed\n");
    assert_eq!(t.read("a.txt"), "fixed\n");
}

#[test]
fn concurrent_commits_land_linearly() {
    let t = T::new();
    let names = ["a", "b", "x", "y"];
    for n in names {
        t.write(&format!("{n}.txt"), &format!("{n} by its own session\n"));
    }
    let procs: Vec<_> = names
        .iter()
        .map(|n| {
            let (msg, path) = (format!("area: {n}"), format!("{n}.txt"));
            cmd(BIN, &t.tmp)
                .args(["commit", "-m", &msg, "--", &path])
                .current_dir(&t.repo)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for p in procs {
        let o = p.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let log = t.sh(&t.repo, &["git", "log", "--format=%P"]);
    assert_eq!(log.lines().count(), 5);
    assert!(
        log.lines()
            .all(|parents| parents.split_whitespace().count() <= 1)
    );
    for n in names {
        assert_eq!(
            t.head_file(&format!("{n}.txt")),
            format!("{n} by its own session\n")
        );
    }
    assert_eq!(t.sh(&t.repo, &["git", "status", "--porcelain"]), "");
}
