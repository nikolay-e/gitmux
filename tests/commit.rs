mod common;

use common::*;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};

fn set_hook(t: &T, name: &str, body: &str) {
    let hook = t.repo.join(".git/hooks").join(name);
    fs::write(&hook, format!("#!/bin/sh\n{body}")).unwrap();
    t.sh(&t.repo, &["chmod", "+x", &hook.to_string_lossy()]);
}

fn write_in(t: &T, path: &str, text: &str) {
    let file = t.repo.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

fn names_in_head(t: &T) -> String {
    t.sh(&t.repo, &["git", "ls-tree", "-r", "--name-only", "HEAD"])
}

fn stderr_of(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A pre-commit hook that logs each run and, for a process started with the gate in its
/// environment, announces itself on one FIFO and waits on the other.
struct Gate {
    ready: PathBuf,
    go: PathBuf,
    log: PathBuf,
}

impl Gate {
    fn new(t: &T) -> Gate {
        let g = Gate {
            ready: t.tmp.join("ready"),
            go: t.tmp.join("go"),
            log: t.tmp.join("hook.log"),
        };
        for fifo in [&g.ready, &g.go] {
            t.sh(&t.tmp, &["mkfifo", &fifo.to_string_lossy()]);
        }
        set_hook(
            t,
            "pre-commit",
            &format!(
                "echo run >> '{}'\nif [ -n \"$MUX_READY\" ]; then echo ready > \"$MUX_READY\"; read _ < \"$MUX_GATE\"; fi\n",
                g.log.display()
            ),
        );
        g
    }

    fn spawn(&self, t: &T, args: &[&str]) -> Child {
        cmd(BIN, &t.tmp)
            .args(args)
            .env("MUX_READY", &self.ready)
            .env("MUX_GATE", &self.go)
            .current_dir(&t.repo)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Returns once the gated hook is running.
    fn wait_ready(&self) {
        fs::read_to_string(&self.ready).unwrap();
    }

    fn release(&self) {
        fs::write(&self.go, "go\n").unwrap();
    }

    fn runs(&self) -> usize {
        fs::read_to_string(&self.log).unwrap().lines().count()
    }
}

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

#[test]
fn a_slow_hook_cannot_touch_the_live_tree() {
    let t = T::new();
    let gate = Gate::new(&t);
    t.write("b.txt", "foreign unstaged edit\n");
    t.write("a.txt", "a2\n");
    let child = gate.spawn(&t, &["commit", "-m", "area: a", "--", "a.txt"]);
    gate.wait_ready();
    t.write("x.txt", "written while the hook runs\n");
    gate.release();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", stderr_of(&o));

    assert_eq!(t.head_file("a.txt"), "a2\n");
    assert_eq!(t.read("b.txt"), "foreign unstaged edit\n");
    assert_eq!(t.read("x.txt"), "written while the hook runs\n");
    assert_eq!(
        t.sh(&t.repo, &["git", "status", "--porcelain"]),
        " M b.txt\n M x.txt\n"
    );
}

#[test]
fn message_hooks_run_in_git_order_and_their_rewrite_is_published() {
    let t = T::new();
    let log = t.tmp.join("order.log");
    let l = log.display();
    set_hook(&t, "pre-commit", &format!("echo pre-commit >> '{l}'\n"));
    set_hook(
        &t,
        "prepare-commit-msg",
        &format!(
            r#"echo "prepare $2" >> '{l}'
printf '\nSigned-off-by: Hook <hook@example.invalid>\n' >> "$1"
"#
        ),
    );
    set_hook(
        &t,
        "commit-msg",
        &format!(
            r#"echo commit-msg >> '{l}'
grep -q '^Signed-off-by' "$1" || exit 1
perl -pi -e 's/^area:/Area:/ if $. == 1' "$1"
"#
        ),
    );
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: change a", "--", "a.txt"]);

    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "pre-commit\nprepare message\ncommit-msg\n"
    );
    assert_eq!(
        t.sh(&t.repo, &["git", "log", "-1", "--format=%B"])
            .trim_end(),
        "Area: change a\n\nSigned-off-by: Hook <hook@example.invalid>"
    );
}

#[test]
fn a_message_the_hooks_empty_publishes_nothing() {
    let t = T::new();
    set_hook(&t, "commit-msg", ": > \"$1\"\n");
    let before = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a2\n");
    let o = t.run(&["commit", "-m", "area: a", "--", "a.txt"], &[]);
    assert!(!o.status.success());
    assert!(stderr_of(&o).contains("empty"), "{}", stderr_of(&o));
    assert_eq!(t.rev(&t.repo, "HEAD"), before);
}

#[test]
fn a_hook_that_fixes_and_passes_lands_the_fix() {
    let t = T::new();
    set_hook(
        &t,
        "pre-commit",
        r#"for f in $(git diff --cached --name-only -- '*.txt'); do
  perl -pi -e 's/fixme/fixed/g' "$f"
  git add "$f"
done
"#,
    );
    t.write("a.txt", "fixme\n");
    t.mux(&["commit", "-m", "area: fix", "--", "a.txt"]);

    assert_eq!(t.head_file("a.txt"), "fixed\n");
    assert_eq!(t.read("a.txt"), "fixed\n");
    assert_eq!(t.sh(&t.repo, &["git", "status", "--porcelain"]), "");
}

#[test]
fn a_hook_that_deletes_a_named_file_refuses() {
    let t = T::new();
    set_hook(&t, "pre-commit", "rm -f a.txt\n");
    let before = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a2\n");
    let o = t.run(&["commit", "-m", "area: a", "--", "a.txt"], &[]);
    assert!(!o.status.success());
    assert!(stderr_of(&o).contains("a.txt"), "{}", stderr_of(&o));
    assert_eq!(t.rev(&t.repo, "HEAD"), before);
    assert_eq!(t.read("a.txt"), "a2\n");
}

#[test]
fn a_deleted_directory_commits() {
    let t = T::new();
    write_in(&t, "d/f1.txt", "one\n");
    write_in(&t, "d/f2.txt", "two\n");
    t.mux(&["commit", "-m", "area: add d", "--", "d"]);
    assert!(names_in_head(&t).contains("d/f2.txt"));

    fs::remove_dir_all(t.repo.join("d")).unwrap();
    t.mux(&["commit", "-m", "area: drop d", "--", "d"]);

    assert_eq!(names_in_head(&t), "a.txt\nb.txt\nx.txt\ny.txt\n");
    assert_eq!(t.sh(&t.repo, &["git", "status", "--porcelain"]), "");
}

#[test]
fn a_move_commits_as_a_rename() {
    let t = T::new();
    fs::rename(t.repo.join("a.txt"), t.repo.join("d.txt")).unwrap();
    t.mux(&["commit", "-m", "area: move", "--", "a.txt", "d.txt"]);

    assert_eq!(
        t.sh(
            &t.repo,
            &["git", "show", "--name-status", "-M", "--format=", "HEAD"]
        ),
        "R100\ta.txt\td.txt\n"
    );
}

#[test]
fn a_publish_conflict_refuses_and_publishes_nothing() {
    let t = T::new();
    let gate = Gate::new(&t);
    t.write("a.txt", "first version\n");
    let first = gate.spawn(&t, &["commit", "-m", "area: first", "--", "a.txt"]);
    gate.wait_ready();
    t.write("a.txt", "second version\n");
    t.mux(&["commit", "-m", "area: second", "--", "a.txt"]);
    let landed = t.rev(&t.repo, "HEAD");
    gate.release();
    let o = first.wait_with_output().unwrap();

    assert!(!o.status.success());
    assert!(stderr_of(&o).contains("conflicts"), "{}", stderr_of(&o));
    assert_eq!(t.rev(&t.repo, "HEAD"), landed);
    assert_eq!(t.head_file("a.txt"), "second version\n");
    assert_eq!(
        t.sh(&t.repo, &["git", "rev-list", "--count", "HEAD"]),
        "2\n"
    );
}

#[test]
fn a_component_owns_a_directory_not_a_longer_name() {
    let t = T::new();
    let gate = Gate::new(&t);
    t.write(
        "components.json",
        r#"{"global": [], "components": [
            {"name": "a", "paths": ["a"], "deps": []},
            {"name": "ab", "paths": ["ab"], "deps": []}]}"#,
    );
    t.mux(&["commit", "-m", "area: components", "--", "components.json"]);
    write_in(&t, "a/f.txt", "in a\n");
    write_in(&t, "ab/g.txt", "in ab\n");

    let first = gate.spawn(&t, &["commit", "-m", "area: a", "--", "a/f.txt"]);
    gate.wait_ready();
    t.mux(&["commit", "-m", "area: ab", "--", "ab/g.txt"]);
    gate.release();
    let o = first.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", stderr_of(&o));

    let head = names_in_head(&t);
    assert!(head.contains("a/f.txt") && head.contains("ab/g.txt"));
    // components.json, the first commit, the second: the landed ab/ change does not re-verify a/.
    assert_eq!(gate.runs(), 3);
}

#[test]
fn a_sparse_checkout_refuses_a_path_outside_its_cone_and_keeps_the_cone() {
    let t = T::new();
    t.sh(
        &t.repo,
        &["git", "sparse-checkout", "set", "--no-cone", "/a.txt"],
    );
    assert!(!t.repo.join("b.txt").exists());
    let before = t.rev(&t.repo, "HEAD");
    let o = t.run(&["commit", "-m", "area: b", "--", "b.txt"], &[]);
    assert!(!o.status.success());
    assert!(stderr_of(&o).contains("sparse"), "{}", stderr_of(&o));
    assert_eq!(t.rev(&t.repo, "HEAD"), before);

    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: a", "--", "a.txt"]);
    assert_eq!(t.head_file("a.txt"), "a2\n");
    assert_eq!(names_in_head(&t), "a.txt\nb.txt\nx.txt\ny.txt\n");
    assert!(!t.repo.join("b.txt").exists());
    assert_eq!(
        t.sh(&t.repo, &["git", "sparse-checkout", "list"]),
        "/a.txt\n"
    );
}

#[test]
fn bracketed_paths_are_literal() {
    let t = T::new();
    set_hook(&t, "pre-commit", "exit 0\n");
    write_in(&t, "app/[id]/page.txt", "the bracketed one\n");
    write_in(&t, "app/d/page.txt", "foreign\n");
    write_in(&t, "app/i/page.txt", "foreign\n");
    t.mux(&["commit", "-m", "area: page", "--", "app/[id]/page.txt"]);

    assert_eq!(
        names_in_head(&t),
        "a.txt\napp/[id]/page.txt\nb.txt\nx.txt\ny.txt\n"
    );
    assert_eq!(
        t.sh(&t.repo, &["git", "status", "--porcelain"]),
        "?? app/d/\n?? app/i/\n"
    );
}

#[test]
fn more_than_a_megabyte_of_paths_commits() {
    let t = T::new();
    set_hook(&t, "pre-commit", "exit 0\n");
    let count = 4200;
    let pad = "x".repeat(240);
    for i in 0..count {
        write_in(&t, &format!("big/{i:05}_{pad}.dat"), "x\n");
    }
    t.mux(&["commit", "-m", "area: many", "--", "big"]);

    assert_eq!(names_in_head(&t).lines().count(), count + 4);
    assert_eq!(t.sh(&t.repo, &["git", "status", "--porcelain"]), "");
}

fn commit_with_env(t: &T, env: &[(&str, &str)], message: &str) -> Output {
    let mut c = cmd(BIN, &t.tmp);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["commit", "-m", message, "--", "a.txt"])
        .current_dir(&t.repo)
        .output()
        .unwrap()
}

/// What Gemini CLI's shell tool injects: an empty hooks path next to a pair the caller means.
const INJECTED: [(&str, &str); 5] = [
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "core.hooksPath"),
    ("GIT_CONFIG_VALUE_0", ""),
    ("GIT_CONFIG_KEY_1", "user.name"),
    ("GIT_CONFIG_VALUE_1", "injected"),
];

#[test]
fn an_injected_hooks_path_does_not_switch_off_the_repositorys_hooks() {
    let t = T::new();
    let before = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "BAD\n");
    let o = commit_with_env(&t, &INJECTED, "area: bad");
    assert!(!o.status.success(), "the failing hook must still refuse");
    assert_eq!(t.rev(&t.repo, "HEAD"), before);

    t.write("a.txt", "good\n");
    let o = commit_with_env(&t, &INJECTED, "area: good");
    assert!(o.status.success(), "{}", stderr_of(&o));
    assert_eq!(t.head_file("a.txt"), "good\n");
    assert_eq!(
        t.sh(&t.repo, &["git", "log", "-1", "--format=%an"]).trim(),
        "injected"
    );
}

#[test]
fn a_hooks_path_given_with_dash_c_is_refused() {
    let t = T::new();
    let before = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "good\n");
    for parameters in ["'core.hooksPath=/x'", "'core.hookspath'='/x'"] {
        let o = commit_with_env(&t, &[("GIT_CONFIG_PARAMETERS", parameters)], "area: x");
        assert!(!o.status.success(), "{parameters}");
        assert!(
            stderr_of(&o).contains("core.hooksPath"),
            "{}",
            stderr_of(&o)
        );
        assert_eq!(t.rev(&t.repo, "HEAD"), before);
    }
    let o = commit_with_env(
        &t,
        &[("GIT_CONFIG_PARAMETERS", "'user.name=other'")],
        "area: ok",
    );
    assert!(o.status.success(), "{}", stderr_of(&o));
}
