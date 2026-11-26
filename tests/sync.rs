mod common;

use common::*;
use serde_json::json;
use std::fs;

#[test]
fn sync_replays_local_commits_and_spares_edited_files() {
    let t = T::new();
    let remote = t.with_remote();
    t.upstream_commit(
        &remote,
        &[("x.txt", Some("x from upstream\n"))],
        "area: upstream x",
    );

    t.write("a.txt", "a local\n");
    t.mux(&["commit", "-m", "area: local a", "--", "a.txt"]);
    t.write("y.txt", "y being edited\n");
    let probe = t.tmp.join("pre-push-ran-in");
    let o = t.run(&["push"], &[("MUX_PROBE", &probe)]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_ne!(
        fs::read_to_string(&probe).unwrap().trim(),
        t.repo.to_string_lossy()
    );

    assert_eq!(t.rev(&t.repo, "HEAD"), t.rev(&remote, "main"));
    assert_eq!(t.read("x.txt"), "x from upstream\n");
    assert_eq!(t.head_file("a.txt"), "a local\n");
    assert_eq!(t.read("y.txt"), "y being edited\n");
    assert_eq!(
        t.sh(&t.repo, &["git", "status", "--porcelain"]),
        " M y.txt\n"
    );

    t.upstream_commit(
        &remote,
        &[("y.txt", Some("y from upstream\n"))],
        "area: upstream y",
    );
    let refused = t.run(&["sync"], &[]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("edited or staged here: y.txt"));
    assert_eq!(t.read("y.txt"), "y being edited\n");
}

#[test]
fn a_sync_with_no_net_change_touches_no_file() {
    let t = T::new();
    let remote = t.with_remote();
    t.upstream_commit(&remote, &[("z.txt", Some("z\n"))], "area: add z");
    t.upstream_commit(&remote, &[("z.txt", None)], "area: drop z");
    t.write("b.txt", "another session's unsaved edit\n");
    t.mux(&["sync"]);
    assert_eq!(t.rev(&t.repo, "HEAD"), t.rev(&remote, "main"));
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
}

#[test]
fn push_refused_by_pre_push_check_sends_nothing() {
    let t = T::new();
    let remote = t.with_remote();
    t.write("a.txt", "NOPUSH\n");
    t.mux(&["commit", "-m", "area: local only", "--", "a.txt"]);
    let refused = t.run(&["push"], &[("MUX_PROBE", &t.tmp.join("probe"))]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("pre-push checks failed"));
    assert_ne!(t.rev(&remote, "main"), t.rev(&t.repo, "HEAD"));
}

#[test]
fn an_interrupted_publish_is_finished_by_the_next_run() {
    for kind in ["commit", "sync"] {
        let t = T::new();
        t.write("a.txt", "a2\n");
        t.mux(&["commit", "-m", "area: a2", "--", "a.txt"]);
        let (old, new) = (t.rev(&t.repo, "HEAD~1"), t.rev(&t.repo, "HEAD"));
        let old_blob = t.rev(&t.repo, &format!("{old}:a.txt"));
        t.sh(
            &t.repo,
            &[
                "git",
                "update-index",
                "--cacheinfo",
                &format!("100644,{old_blob},a.txt"),
            ],
        );
        if kind == "sync" {
            t.write("a.txt", "a\n");
        }
        let journal = json!({"kind": kind, "old": old, "new": new, "paths": ["a.txt"]});
        fs::write(t.state().join("publish.1.json"), journal.to_string()).unwrap();

        assert!(
            String::from_utf8_lossy(&t.mux(&["status"]).stderr)
                .contains("finished the interrupted")
        );
        assert_eq!(t.read("a.txt"), "a2\n", "{kind}");
        assert_eq!(
            t.sh(&t.repo, &["git", "status", "--porcelain"]),
            "",
            "{kind}"
        );
        assert!(!t.state().join("publish.1.json").exists());
    }
}
