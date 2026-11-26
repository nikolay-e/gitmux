mod common;

use common::*;
use serde_json::json;
use std::fs;
use std::path::Path;

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

/// An upstream commit that may create directories, unlike the harness's `upstream_commit`.
fn upstream(t: &T, remote: &Path, edits: &[(&str, Option<&str>)], message: &str) {
    let other = t.tmp.join("other");
    if !other.exists() {
        t.sh(
            &t.tmp,
            &[
                "git",
                "clone",
                "-q",
                &remote.to_string_lossy(),
                &other.to_string_lossy(),
            ],
        );
        configure(t, &other);
    }
    t.sh(&other, &["git", "pull", "-q", "--rebase"]);
    for (path, text) in edits {
        match text {
            Some(text) => {
                fs::create_dir_all(other.join(path).parent().unwrap()).unwrap();
                fs::write(other.join(path), text).unwrap();
            }
            None => fs::remove_file(other.join(path)).unwrap(),
        }
    }
    t.sh(&other, &["git", "add", "-A"]);
    t.sh(
        &other,
        &["git", "commit", "-q", "--no-verify", "-m", message],
    );
    t.sh(&other, &["git", "push", "-q", "origin", "main"]);
}

fn journals(t: &T) -> usize {
    fs::read_dir(t.state())
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("publish.")
        })
        .count()
}

fn porcelain(t: &T) -> String {
    t.sh(&t.repo, &["git", "status", "--porcelain"])
}

/// A sync that moved the branch to a commit changing a.txt and died before the index and the
/// file followed: the index holds the old blob, the file `live`, and the journal is left behind.
fn interrupted_sync(t: &T, live: &str) {
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
    t.write("a.txt", live);
    let journal = json!({"kind": "sync", "old": old, "new": new, "paths": ["a.txt"]});
    fs::write(t.state().join("publish.1.json"), journal.to_string()).unwrap();
}

#[test]
fn a_sparse_cone_survives_sync() {
    let t = T::new();
    let remote = t.with_remote();
    upstream(
        &t,
        &remote,
        &[("d1/a.txt", Some("a1\n")), ("d2/b.txt", Some("b1\n"))],
        "area: dirs",
    );
    t.mux(&["sync"]);
    t.sh(&t.repo, &["git", "sparse-checkout", "set", "--cone", "d1"]);
    assert!(!t.repo.join("d2").exists());

    upstream(
        &t,
        &remote,
        &[
            ("d1/c.txt", Some("c\n")),
            ("d2/new.txt", Some("n\n")),
            ("d2/b.txt", Some("b2\n")),
        ],
        "area: more",
    );
    t.mux(&["sync"]);
    assert_eq!(t.rev(&t.repo, "HEAD"), t.rev(&remote, "main"));
    assert_eq!(t.read("d1/c.txt"), "c\n");
    assert!(!t.repo.join("d2").exists(), "outside the cone");
    assert_eq!(
        t.sh(&t.repo, &["git", "sparse-checkout", "list"]).trim(),
        "d1"
    );
    assert_eq!(porcelain(&t), "");
    let flags = t.sh(&t.repo, &["git", "ls-files", "-t", "d2"]);
    assert!(flags.contains("S d2/new.txt"), "{flags}");
    assert!(flags.contains("S d2/b.txt"), "{flags}");
}

#[test]
fn upstream_paths_arrive_literally() {
    let t = T::new();
    let remote = t.with_remote();
    upstream(
        &t,
        &remote,
        &[("app/[id]/page.txt", Some("v1\n"))],
        "area: page",
    );
    t.mux(&["sync"]);
    assert_eq!(t.read("app/[id]/page.txt"), "v1\n");
    upstream(
        &t,
        &remote,
        &[("app/[id]/page.txt", Some("v2\n"))],
        "area: page v2",
    );
    t.mux(&["sync"]);
    assert_eq!(t.read("app/[id]/page.txt"), "v2\n");
    assert_eq!(porcelain(&t), "");
}

#[test]
fn an_upstream_commit_of_over_a_megabyte_of_paths_syncs() {
    let t = T::new();
    let remote = t.with_remote();
    upstream(&t, &remote, &[("seed.txt", Some("s\n"))], "area: seed");
    let other = t.tmp.join("other");
    let dir = [&"a".repeat(200), &"b".repeat(200)];
    let rel = Path::new(dir[0]).join(dir[1]);
    fs::create_dir_all(other.join(&rel)).unwrap();
    for i in 0..3000 {
        fs::write(other.join(&rel).join(format!("f{i:04}.txt")), "x\n").unwrap();
    }
    t.sh(&other, &["git", "add", "-A"]);
    t.sh(
        &other,
        &["git", "commit", "-q", "--no-verify", "-m", "area: many"],
    );
    t.sh(&other, &["git", "push", "-q", "origin", "main"]);

    t.mux(&["sync"]);
    assert_eq!(t.rev(&t.repo, "HEAD"), t.rev(&remote, "main"));
    assert_eq!(fs::read_dir(t.repo.join(&rel)).unwrap().count(), 3000);
    assert_eq!(porcelain(&t), "");
}

#[test]
fn an_upstream_deletion_syncs_and_a_heal_of_one_finishes() {
    let t = T::new();
    let remote = t.with_remote();
    upstream(&t, &remote, &[("x.txt", None)], "area: drop x");
    t.mux(&["sync"]);
    assert!(!t.repo.join("x.txt").exists());
    assert_eq!(porcelain(&t), "");
    assert_eq!(journals(&t), 0);

    let t = T::new();
    fs::remove_file(t.repo.join("x.txt")).unwrap();
    t.mux(&["commit", "-m", "area: drop x", "--", "x.txt"]);
    let (old, new) = (t.rev(&t.repo, "HEAD~1"), t.rev(&t.repo, "HEAD"));
    let blob = t.rev(&t.repo, &format!("{old}:x.txt"));
    t.sh(
        &t.repo,
        &[
            "git",
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},x.txt"),
        ],
    );
    t.write("x.txt", "x\n");
    let journal = json!({"kind": "sync", "old": old, "new": new, "paths": ["x.txt"]});
    fs::write(t.state().join("publish.1.json"), journal.to_string()).unwrap();

    let o = t.mux(&["status"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("finished the interrupted"));
    assert!(!t.repo.join("x.txt").exists());
    assert_eq!(porcelain(&t), "");
    assert_eq!(journals(&t), 0);
}

#[test]
fn a_sync_journal_whose_live_file_changed_is_left_alone() {
    let t = T::new();
    interrupted_sync(&t, "edited meanwhile\n");
    let o = t.mux(&["status"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("left alone"), "{err}");
    assert!(err.contains("a.txt"), "{err}");
    assert_eq!(t.read("a.txt"), "edited meanwhile\n");
    assert_eq!(journals(&t), 0);
}

#[test]
fn sync_and_heal_run_no_post_checkout_hook() {
    let t = T::new();
    let log = t.tmp.join("post-checkout.log");
    let hook = t.repo.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n[ \"$(git rev-parse --show-toplevel)\" = '{}' ] && echo ran >> '{}'\nexit 0\n",
            t.repo.display(),
            log.display()
        ),
    )
    .unwrap();
    t.sh(&t.repo, &["chmod", "+x", &hook.to_string_lossy()]);
    let remote = t.with_remote();
    upstream(
        &t,
        &remote,
        &[("x.txt", Some("x2\n")), ("n/new.txt", Some("n\n"))],
        "area: x2",
    );
    t.mux(&["sync"]);
    assert_eq!(t.read("x.txt"), "x2\n");
    interrupted_sync(&t, "a\n");
    t.mux(&["status"]);
    assert_eq!(t.read("a.txt"), "a2\n");
    assert!(!log.exists(), "post-checkout ran in the live tree");
}

#[test]
fn push_names_a_tag_and_checks_it_in_a_slot() {
    let t = T::new();
    let remote = t.with_remote();
    let hook = t.repo.join(".git/hooks/pre-push");
    fs::write(
        &hook,
        "#!/bin/sh\ncat > \"$MUX_PROBE.stdin\"\ngit rev-parse --show-toplevel > \"$MUX_PROBE\"\n",
    )
    .unwrap();
    t.sh(&t.repo, &["chmod", "+x", &hook.to_string_lossy()]);
    t.sh(&t.repo, &["git", "tag", "-a", "v1", "-m", "release"]);
    let tag = t.rev(&t.repo, "v1");

    let probe = t.tmp.join("probe");
    let o = t.run(&["push", "v1"], &[("MUX_PROBE", &probe)]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_ne!(
        fs::read_to_string(&probe).unwrap().trim(),
        t.repo.to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(t.tmp.join("probe.stdin")).unwrap(),
        format!("refs/tags/v1 {tag} refs/tags/v1 {}\n", "0".repeat(40))
    );
    assert_eq!(t.rev(&remote, "refs/tags/v1"), tag);
}

#[test]
fn a_ref_that_fails_pre_push_leaves_the_remote_untouched() {
    let t = T::new();
    let remote = t.with_remote();
    let before = t.rev(&remote, "main");
    t.write("a.txt", "NOPUSH\n");
    t.mux(&["commit", "-m", "area: local only", "--", "a.txt"]);
    t.sh(&t.repo, &["git", "tag", "bad"]);
    let o = t.run(&["push", "bad"], &[("MUX_PROBE", &t.tmp.join("probe"))]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("pre-push checks failed"));
    assert!(!t.git_ok(&remote, &["rev-parse", "-q", "--verify", "refs/tags/bad"]));
    assert_eq!(t.rev(&remote, "main"), before);
}

#[test]
fn a_hooks_path_given_with_dash_c_refuses_push_and_status_and_writes_nothing() {
    let t = T::new();
    let remote = t.with_remote();
    t.write("a.txt", "local\n");
    t.mux(&["commit", "-m", "area: local a", "--", "a.txt"]);
    let (pushed, hooks) = (t.rev(&remote, "main"), t.tmp.join("elsewhere"));
    fs::create_dir(&hooks).unwrap();
    let parameters = format!("'core.hookspath'='{}'", hooks.display());
    for args in [&["push"][..], &["status"]] {
        let o = t.run(args, &[("GIT_CONFIG_PARAMETERS", Path::new(&parameters))]);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success(), "{args:?}: {err}");
        assert!(err.contains("core.hooksPath"), "{args:?}: {err}");
        assert_eq!(fs::read_dir(&hooks).unwrap().count(), 0, "{args:?}");
    }
    assert_eq!(t.rev(&remote, "main"), pushed);
}
