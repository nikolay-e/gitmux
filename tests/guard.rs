mod common;

use common::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

fn git_env(t: &T, dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> Output {
    let mut c = cmd("git", &t.tmp);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(args).current_dir(dir).output().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn executable(t: &T, path: &Path) {
    t.sh(&t.tmp, &["chmod", "+x", &path.to_string_lossy()]);
}

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
    let refused: [&[&str]; 4] = [
        &["commit", "-qam", "area: x"],
        &["commit", "-qam", "area: x", "--no-verify"],
        &["stash"],
        &["reset", "--soft", ahead.trim()],
    ];
    for args in refused {
        assert!(!t.git_ok(&t.repo, args), "{args:?}");
    }
    // The pre-push test hook also fails here (no MUX_PROBE), so only the guard's own words prove
    // that the guard is what refused.
    let plain = git_env(&t, &t.repo, &[], &["push", "-q", &target, "HEAD"]);
    assert!(!plain.status.success());
    assert!(stderr(&plain).contains("plain git push in the shared checkout"));
    assert!(stderr(&plain).contains("git mux push"));
    // `git mux push` runs pre-push in a slot and pushes with --no-verify, so a pre-push hook in
    // the live tree has no legitimate caller: the override does not open it.
    let overridden = git_env(
        &t,
        &t.repo,
        &[("GIT_MUX", Path::new("1"))],
        &["push", "-q", &target, "HEAD:refs/heads/main"],
    );
    assert!(stderr(&overridden).contains("git mux push"));
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

#[test]
fn the_override_skips_the_reference_guard_and_never_a_hook() {
    let t = T::new();
    let head = t.rev(&t.repo, "HEAD");
    let mux = [("GIT_MUX", Path::new("1"))];
    // The test pre-commit hook rewrites `fixme`: unchanged text proves it never ran in the tree.
    t.write("a.txt", "fixme\n");
    let refused = git_env(&t, &t.repo, &mux, &["commit", "-qam", "area: x"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("plain git commit in the shared checkout"));
    assert_eq!(t.read("a.txt"), "fixme\n");
    assert_eq!(t.rev(&t.repo, "HEAD"), head);

    let deliberate = git_env(
        &t,
        &t.repo,
        &mux,
        &["commit", "-qam", "area: x", "--no-verify"],
    );
    assert!(deliberate.status.success(), "{}", stderr(&deliberate));
    assert_ne!(t.rev(&t.repo, "HEAD"), head);
    assert_eq!(t.head_file("a.txt"), "fixme\n");
}

#[test]
fn hook_lines_are_current_idempotent_and_replace_older_ones() {
    let t = T::new();
    let hooks = t.repo.join(".git/hooks");
    let text = fs::read_to_string(hooks.join("pre-commit")).unwrap();
    let (shebang, rest) = text.split_once('\n').unwrap();
    let old = "\"$(dirname \"$0\")/mux-guard\" commit || exit 1  # git-mux\n";
    fs::write(hooks.join("pre-commit"), format!("{shebang}\n{old}{rest}")).unwrap();
    fs::write(hooks.join("mux-guard"), "#!/bin/sh\nexit 0\n").unwrap();

    t.mux(&["install"]);
    let current = fs::read_to_string(hooks.join("pre-commit")).unwrap();
    let lines: Vec<&str> = current.lines().collect();
    assert_eq!(lines[0], "#!/bin/sh");
    assert!(lines[1].ends_with("\"$g\" commit || exit 1  # git-mux"));
    assert_eq!(current.matches("# git-mux").count(), 1);
    assert!(!current.contains("mux-guard"));
    assert!(!hooks.join("mux-guard").exists());
    let push = fs::read_to_string(hooks.join("pre-push")).unwrap();
    assert!(push.contains("\"$g\" push || exit 1  # git-mux"));
    let guard = fs::read_to_string(t.state().join("guard")).unwrap();
    // Only a guard line marked `open` (prepare-commit-msg's) lets the override through.
    assert_eq!(guard.matches("GIT_MUX").count(), 1);
    assert!(guard.contains("[ \"$2\" = open ] && [ -n \"$GIT_MUX\" ] && exit 0"));

    t.mux(&["install"]);
    assert_eq!(
        fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        current
    );
    assert_eq!(fs::read_to_string(hooks.join("pre-push")).unwrap(), push);
}

#[test]
fn a_husky_style_hooks_dir_gets_the_line() {
    let t = T::new();
    let dir = t.repo.join(".husky/_");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".gitignore"), "*\n").unwrap();
    fs::write(dir.join("pre-commit"), "#!/usr/bin/env sh\nexit 0\n").unwrap();
    executable(&t, &dir.join("pre-commit"));
    t.sh(&t.repo, &["git", "config", "core.hooksPath", ".husky/_"]);

    t.mux(&["install"]);
    let hook = fs::read_to_string(dir.join("pre-commit")).unwrap();
    assert!(hook.starts_with("#!/usr/bin/env sh\n"));
    assert!(hook.contains("\"$g\" commit || exit 1  # git-mux"));
    assert!(dir.join("reference-transaction").is_file());
    t.write("a.txt", "x\n");
    let refused = git_env(&t, &t.repo, &[], &["commit", "-qam", "area: x"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("plain git commit in the shared checkout"));
}

#[test]
fn a_tracked_hooks_dir_is_untouched_and_install_prints_the_line() {
    let t = T::new();
    let dir = t.repo.join(".githooks");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
    executable(&t, &dir.join("pre-commit"));
    t.sh(&t.repo, &["git", "config", "core.hooksPath", ".githooks"]);
    t.sh(&t.repo, &["git", "add", ".githooks"]);
    let mux = [("GIT_MUX", Path::new("1"))];
    let landed = git_env(
        &t,
        &t.repo,
        &mux,
        &["commit", "-q", "--no-verify", "-m", "area: hooks"],
    );
    assert!(landed.status.success(), "{}", stderr(&landed));
    let before = fs::read(dir.join("pre-commit")).unwrap();

    let o = t.mux(&["install"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        said.contains("\"$g\" commit || exit 1  # git-mux"),
        "{said}"
    );
    assert_eq!(fs::read(dir.join("pre-commit")).unwrap(), before);
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["pre-commit"]);
}

#[test]
fn a_commit_msg_hook_without_a_pre_commit_file_is_guarded_too() {
    let t = T::new();
    let probe = t.tmp.join("commit-msg-probe");
    let hooks = t.repo.join(".git/hooks");
    fs::remove_file(hooks.join("pre-commit")).unwrap();
    fs::write(
        hooks.join("commit-msg"),
        "#!/bin/sh\necho ran > \"$MUX_PROBE\"\n",
    )
    .unwrap();
    t.mux(&["install"]);
    assert!(
        fs::read_to_string(hooks.join("commit-msg"))
            .unwrap()
            .contains("\"$g\" commit || exit 1  # git-mux")
    );

    let head = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a2\n");
    let refused = git_env(
        &t,
        &t.repo,
        &[("MUX_PROBE", &probe)],
        &["commit", "-qam", "area: x"],
    );
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("plain git commit in the shared checkout"));
    assert!(
        !probe.exists(),
        "the repository's commit-msg ran in the live tree"
    );
    assert_eq!(t.rev(&t.repo, "HEAD"), head);

    let deliberate = git_env(
        &t,
        &t.repo,
        &[("MUX_PROBE", &probe), ("GIT_MUX", Path::new("1"))],
        &["commit", "-qam", "area: x", "--no-verify"],
    );
    assert!(deliberate.status.success(), "{}", stderr(&deliberate));
    assert!(!probe.exists());
}

#[test]
fn a_perl_pre_commit_hook_is_chained() {
    let t = T::new();
    let probe = t.tmp.join("perl-probe");
    let hook = t.repo.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/usr/bin/perl\nopen(my $f, '>', $ENV{MUX_PROBE}) or die;\nprint $f 'ran';\nclose($f);\n",
    )
    .unwrap();
    t.mux(&["install"]);
    assert!(
        t.repo
            .join(".git/hooks/gitmux-chained/pre-commit")
            .is_file()
    );

    t.write("a.txt", "a2\n");
    let refused = git_env(
        &t,
        &t.repo,
        &[("MUX_PROBE", &probe)],
        &["commit", "-qam", "area: x"],
    );
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("plain git commit in the shared checkout"));
    assert!(!probe.exists());

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
    let landed = git_env(
        &t,
        &wt,
        &[("MUX_PROBE", &probe)],
        &["commit", "-qam", "area: branch work"],
    );
    assert!(landed.status.success(), "{}", stderr(&landed));
    assert_eq!(fs::read_to_string(&probe).unwrap(), "ran");
}

#[test]
fn a_foreign_reference_transaction_hook_keeps_running() {
    let t = T::new();
    let log = t.tmp.join("refs-log");
    let hook = t.repo.join(".git/hooks/reference-transaction");
    fs::write(&hook, format!("#!/bin/sh\ncat > '{}'.$1\n", log.display())).unwrap();
    executable(&t, &hook);
    t.mux(&["install"]);
    assert!(
        t.repo
            .join(".git/hooks/gitmux-chained/reference-transaction")
            .is_file()
    );

    let head = t.rev(&t.repo, "HEAD");
    assert!(t.git_ok(&t.repo, &["update-ref", "refs/heads/side", &head]));
    let prepared = fs::read_to_string(state_log(&log, "prepared")).unwrap();
    let committed = fs::read_to_string(state_log(&log, "committed")).unwrap();
    assert_eq!(prepared, committed);
    assert!(
        prepared.ends_with(&format!(" {head} refs/heads/side\n")),
        "{prepared}"
    );

    let ahead = t.sh(
        &t.repo,
        &["git", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "x"],
    );
    assert!(!t.git_ok(&t.repo, &["update-ref", "refs/heads/main", ahead.trim()]));
    assert_eq!(t.rev(&t.repo, "HEAD"), head);
}

fn state_log(log: &Path, state: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.{state}", log.display()))
}

#[test]
fn a_sha256_repository_commits_deletes_branches_and_refuses_resets() {
    let t = T::new();
    let repo = t.tmp.join("sha256");
    fs::create_dir_all(&repo).unwrap();
    t.sh(
        &repo,
        &["git", "init", "-q", "-b", "main", "--object-format=sha256"],
    );
    configure(&t, &repo);
    t.sh(
        &repo,
        &[
            "git",
            "config",
            "gitmux.slotDir",
            &t.tmp.join("slots-sha256").to_string_lossy(),
        ],
    );
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    t.sh(&repo, &["git", "add", "a.txt"]);
    t.sh(&repo, &["git", "commit", "-q", "--no-verify", "-m", "init"]);
    let mux = |args: &[&str]| {
        let o = cmd(BIN, &t.tmp)
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&o.stdout),
            stderr(&o)
        );
    };
    mux(&["install"]);

    fs::write(repo.join("a.txt"), "a2\n").unwrap();
    mux(&["commit", "-m", "area: change a", "--", "a.txt"]);
    let head = t.rev(&repo, "HEAD");
    assert_eq!(head.len(), 64);
    assert_eq!(t.sh(&repo, &["git", "show", "HEAD:a.txt"]), "a2\n");

    t.sh(&repo, &["git", "branch", "topic"]);
    assert!(t.git_ok(&repo, &["branch", "-D", "topic"]));
    assert!(!t.git_ok(&repo, &["reset", "--soft", "HEAD~"]));
    assert!(!t.git_ok(&repo, &["update-ref", "-d", "refs/heads/main"]));
    assert!(!t.git_ok(&repo, &["update-ref", "-d", "refs/heads/main", &head]));
    assert!(t.git_ok(&repo, &["pack-refs", "--all"]));
    assert!(t.git_ok(&repo, &["gc", "-q"]));
    assert_eq!(t.rev(&repo, "HEAD"), head);
}

#[test]
fn the_shared_branch_cannot_be_deleted_but_ref_maintenance_works() {
    let t = T::new();
    let head = t.rev(&t.repo, "HEAD");
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
    for dir in [&t.repo, &wt] {
        assert!(!t.git_ok(dir, &["update-ref", "-d", "refs/heads/main"]));
        assert!(!t.git_ok(dir, &["update-ref", "-d", "refs/heads/main", &head]));
    }
    assert_eq!(t.rev(&t.repo, "main"), head);

    // pack-refs and gc delete the loose file of a branch that is packed at the same value,
    // whether or not the branch moved since the last pack.
    assert!(t.git_ok(&t.repo, &["pack-refs", "--all"]));
    for round in 0..2 {
        t.write("a.txt", &format!("round {round}\n"));
        t.mux(&["commit", "-m", "area: moved", "--", "a.txt"]);
        let moved = t.rev(&t.repo, "main");
        assert!(
            t.git_ok(&t.repo, &["pack-refs", "--all"]),
            "pack-refs {round}"
        );
        assert_eq!(t.rev(&t.repo, "main"), moved);
        t.write("a.txt", &format!("again {round}\n"));
        t.mux(&["commit", "-m", "area: moved again", "--", "a.txt"]);
        let moved = t.rev(&t.repo, "main");
        assert!(t.git_ok(&t.repo, &["gc", "-q"]), "gc {round}");
        assert_eq!(t.rev(&t.repo, "main"), moved);
    }

    t.sh(&t.repo, &["git", "branch", "loose"]);
    t.sh(&t.repo, &["git", "branch", "packed"]);
    t.sh(&t.repo, &["git", "pack-refs", "--all"]);
    t.sh(&t.repo, &["git", "branch", "other"]);
    for name in ["loose", "packed", "other"] {
        assert!(t.git_ok(
            &t.repo,
            &["update-ref", "-d", &format!("refs/heads/{name}")]
        ));
    }

    let deliberate = git_env(
        &t,
        &t.repo,
        &[("GIT_MUX", Path::new("1"))],
        &["update-ref", "-d", "refs/heads/main"],
    );
    assert!(deliberate.status.success(), "{}", stderr(&deliberate));
    assert!(!t.git_ok(&t.repo, &["rev-parse", "-q", "--verify", "refs/heads/main"]));
}

fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn logging_hook(t: &T, path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    executable(t, path);
}

#[test]
fn no_hook_file_runs_in_the_live_tree_whatever_files_exist() {
    let t = T::new();
    t.mux(&["uninstall"]);
    let hooks = t.repo.join(".git/hooks");
    for name in ["pre-commit", "commit-msg", "pre-push"] {
        fs::remove_file(hooks.join(name)).unwrap();
    }
    let record = "git rev-parse --show-toplevel >> \"$MUX_PROBE\"";
    logging_hook(&t, &hooks.join("prepare-commit-msg"), record);
    logging_hook(&t, &hooks.join("commit-msg"), record);
    let before = snapshot(&hooks);

    t.mux(&["install"]);
    let created = fs::read_to_string(hooks.join("pre-commit")).unwrap();
    assert!(created.starts_with("#!/bin/sh\n"));
    assert!(created.contains("\"$g\" commit || exit 1  # git-mux"));
    assert_eq!(
        fs::metadata(hooks.join("pre-commit"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    let prepare = fs::read_to_string(hooks.join("prepare-commit-msg")).unwrap();
    assert!(prepare.contains("\"$g\" commit open || exit 1  # git-mux"));
    t.mux(&["install"]);
    assert_eq!(
        fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        created
    );

    let probe = t.tmp.join("live-probe");
    let head = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a2\n");
    let env = [("MUX_PROBE", probe.as_path())];
    let mux = [("MUX_PROBE", probe.as_path()), ("GIT_MUX", Path::new("1"))];
    let attempts: [(bool, &[&str]); 3] = [
        (false, &["commit", "-qam", "area: x"]),
        (false, &["commit", "-qam", "area: x", "--no-verify"]),
        (true, &["commit", "-qam", "area: x"]),
    ];
    for (override_set, args) in attempts {
        let vars = if override_set { &mux[..] } else { &env[..] };
        let refused = git_env(&t, &t.repo, vars, args);
        assert!(!refused.status.success(), "{args:?} landed");
        assert!(
            stderr(&refused).contains("plain git commit in the shared checkout"),
            "{args:?}: {}",
            stderr(&refused)
        );
        assert!(!probe.exists(), "{args:?} ran a hook in the live tree");
        assert_eq!(t.rev(&t.repo, "HEAD"), head);
    }

    let deliberate = git_env(
        &t,
        &t.repo,
        &mux,
        &["commit", "-qam", "area: x", "--no-verify"],
    );
    assert!(deliberate.status.success(), "{}", stderr(&deliberate));
    assert_ne!(t.rev(&t.repo, "HEAD"), head);
    assert_eq!(
        fs::read_to_string(&probe).unwrap().trim(),
        t.repo.to_string_lossy()
    );

    let slot_probe = t.tmp.join("slot-probe");
    t.write("b.txt", "b2\n");
    let o = t.run(
        &["commit", "-m", "area: b2", "--", "b.txt"],
        &[("MUX_PROBE", slot_probe.as_path())],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let ran = fs::read_to_string(&slot_probe).unwrap();
    assert_eq!(ran.lines().count(), 2, "{ran}");
    assert!(ran.lines().all(|l| l.ends_with("/mux-verify-0")), "{ran}");

    t.mux(&["uninstall"]);
    assert_eq!(snapshot(&hooks), before);
    assert!(!hooks.join("pre-commit").exists());
}

#[test]
fn a_guarded_prepare_commit_msg_still_runs_between_pre_commit_and_commit_msg_in_the_slot() {
    let t = T::new();
    let hooks = t.repo.join(".git/hooks");
    logging_hook(
        &t,
        &hooks.join("prepare-commit-msg"),
        "printf 'area: ' | cat - \"$1\" > \"$1.new\" && mv \"$1.new\" \"$1\"",
    );
    t.mux(&["install"]);
    let head = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "fixme\n");
    let plain = git_env(
        &t,
        &t.repo,
        &[],
        &["commit", "-qam", "plain subject", "--no-verify"],
    );
    assert!(!plain.status.success());
    assert!(stderr(&plain).contains("plain git commit in the shared checkout"));
    assert_eq!(t.rev(&t.repo, "HEAD"), head);

    t.mux(&["commit", "-m", "plain subject", "--", "a.txt"]);
    assert_eq!(
        t.sh(&t.repo, &["git", "log", "-1", "--format=%s"]),
        "area: plain subject\n"
    );
    assert_eq!(t.head_file("a.txt"), "fixed\n");
}
