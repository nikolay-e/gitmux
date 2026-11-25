//! git-mux against real repositories, real git hooks and real concurrent processes: the shared
//! checkout must keep every other session's unstaged and staged work exactly as it was.
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_git-mux");

const PRE_COMMIT: &str = r#"#!/bin/sh
files=$(git diff --cached --name-only --diff-filter=ACMR -- '*.txt')
[ -n "$files" ] || exit 0
if grep -l BAD $files; then exit 1; fi
perl -pi -e 's/fixme/fixed/g' $files
git diff --quiet -- $files || exit 1
if grep -rl --include='*.txt' BROKEN .; then exit 1; fi
"#;
const COMMIT_MSG: &str = "#!/bin/sh\ngrep -qE '^[a-z]+: ' \"$1\"\n";
const PRE_PUSH: &str = "#!/bin/sh\ncat >/dev/null\n! git grep -q NOPUSH HEAD -- '*.txt' && git rev-parse --show-toplevel > \"$MUX_PROBE\"\n";

struct T {
    tmp: PathBuf,
    repo: PathBuf,
}

static N: AtomicUsize = AtomicUsize::new(0);

fn cmd(program: &str, tmp: &Path) -> Command {
    let mut c = Command::new(program);
    for (k, _) in std::env::vars() {
        if k.starts_with("GIT_") || k.starts_with("CLAUDE") || k.starts_with("CODEX") {
            c.env_remove(k);
        }
    }
    c.env("HOME", tmp.join("home"))
        .env("XDG_CONFIG_HOME", tmp.join("config"))
        .env("GIT_CONFIG_NOSYSTEM", "1");
    c
}

fn configure(t: &T, dir: &Path) {
    for (k, v) in [
        ("user.name", "t"),
        ("user.email", "t@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        t.sh(dir, &["git", "config", k, v]);
    }
}

impl T {
    fn new() -> T {
        let tmp = std::env::temp_dir().join(format!(
            "gitmux-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("repo")).unwrap();
        fs::create_dir_all(tmp.join("home")).unwrap();
        let tmp = fs::canonicalize(tmp).unwrap();
        let t = T {
            repo: tmp.join("repo"),
            tmp,
        };
        t.sh(&t.repo, &["git", "init", "-q", "-b", "main"]);
        configure(&t, &t.repo);
        t.sh(
            &t.repo,
            &[
                "git",
                "config",
                "gitmux.slotDir",
                &t.tmp.join("slots").to_string_lossy(),
            ],
        );
        for n in ["a", "b", "x", "y"] {
            t.write(&format!("{n}.txt"), &format!("{n}\n"));
        }
        t.sh(&t.repo, &["git", "add", "."]);
        t.sh(
            &t.repo,
            &["git", "commit", "-q", "--no-verify", "-m", "init"],
        );
        for (name, text) in [
            ("pre-commit", PRE_COMMIT),
            ("commit-msg", COMMIT_MSG),
            ("pre-push", PRE_PUSH),
        ] {
            let hook = t.repo.join(".git/hooks").join(name);
            fs::write(&hook, text).unwrap();
            t.sh(&t.repo, &["chmod", "+x", &hook.to_string_lossy()]);
        }
        t.mux(&["status"]);
        t
    }

    fn sh(&self, dir: &Path, args: &[&str]) -> String {
        let o = cmd(args[0], &self.tmp)
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn git_ok(&self, dir: &Path, args: &[&str]) -> bool {
        cmd("git", &self.tmp)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success()
    }

    fn run(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut c = cmd(BIN, &self.tmp);
        for (k, v) in env {
            c.env(k, v);
        }
        c.args(args).current_dir(&self.repo).output().unwrap()
    }

    fn mux(&self, args: &[&str]) -> Output {
        let o = self.run(args, &[]);
        assert!(
            o.status.success(),
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.repo.join(path), text).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.repo.join(path)).unwrap()
    }

    fn head_file(&self, path: &str) -> String {
        self.sh(&self.repo, &["git", "show", &format!("HEAD:{path}")])
    }

    fn rev(&self, dir: &Path, rev: &str) -> String {
        self.sh(dir, &["git", "rev-parse", rev]).trim().to_string()
    }

    fn state(&self) -> PathBuf {
        self.repo.join(".git/gitmux")
    }

    fn leases(&self) -> Value {
        fs::read(self.state().join("leases.json"))
            .map(|d| serde_json::from_slice(&d).unwrap())
            .unwrap_or(json!({}))
    }

    /// The denial's stderr, the PostToolUse note, or None when the call passes silently.
    fn hook_at(
        &self,
        session: &str,
        tool: &str,
        input: Value,
        event: &str,
        call: &str,
    ) -> Option<String> {
        let payload = json!({"session_id": session, "cwd": self.repo, "tool_name": tool, "tool_input": input,
                             "hook_event_name": event, "tool_use_id": call});
        let o = self.hook_raw(&payload.to_string());
        if o.status.code() == Some(2) {
            return Some(String::from_utf8_lossy(&o.stderr).into_owned());
        }
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let out = String::from_utf8_lossy(&o.stdout);
        (!out.trim().is_empty()).then(|| {
            serde_json::from_str::<Value>(&out).unwrap()["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_string()
        })
    }

    fn hook(&self, session: &str, tool: &str, input: Value) -> Option<String> {
        self.hook_at(session, tool, input, "PreToolUse", "t")
    }

    fn hook_raw(&self, payload: &str) -> Output {
        use std::io::Write;
        let mut child = cmd(BIN, &self.tmp)
            .arg("hook")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn with_remote(&self) -> PathBuf {
        let remote = self.tmp.join("remote.git");
        self.sh(
            &self.tmp,
            &[
                "git",
                "clone",
                "-q",
                "--bare",
                &self.repo.to_string_lossy(),
                &remote.to_string_lossy(),
            ],
        );
        self.sh(
            &self.repo,
            &["git", "remote", "add", "origin", &remote.to_string_lossy()],
        );
        self.sh(&self.repo, &["git", "fetch", "-q", "origin"]);
        self.sh(
            &self.repo,
            &["git", "branch", "-q", "--set-upstream-to=origin/main"],
        );
        remote
    }

    fn upstream_commit(&self, remote: &Path, edits: &[(&str, Option<&str>)], message: &str) {
        let other = self.tmp.join("other");
        if !other.exists() {
            self.sh(
                &self.tmp,
                &[
                    "git",
                    "clone",
                    "-q",
                    &remote.to_string_lossy(),
                    &other.to_string_lossy(),
                ],
            );
            configure(self, &other);
        }
        self.sh(&other, &["git", "pull", "-q", "--rebase"]);
        for (path, text) in edits {
            match text {
                Some(text) => fs::write(other.join(path), text).unwrap(),
                None => fs::remove_file(other.join(path)).unwrap(),
            }
        }
        self.sh(&other, &["git", "add", "-A"]);
        self.sh(
            &other,
            &["git", "commit", "-q", "--no-verify", "-m", message],
        );
        self.sh(&other, &["git", "push", "-q", "origin", "main"]);
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.tmp);
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
fn leases_keep_sessions_off_each_others_files() {
    let t = T::new();
    let a = t.repo.join("a.txt").to_string_lossy().into_owned();
    assert_eq!(t.hook("A", "Edit", json!({"file_path": a})), None);
    t.write("a.txt", "A is editing\n");
    assert!(
        t.hook("B", "Write", json!({"file_path": a}))
            .unwrap()
            .contains("another live agent")
    );
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-a\n+b\n*** End Patch\n";
    assert!(
        t.hook("B", "apply_patch", json!({"command": patch}))
            .unwrap()
            .contains("another live agent")
    );
    for steal in [
        "git mux commit -m 'area: steal' -- a.txt",
        "git-mux commit -m 'area: steal' a.txt",
    ] {
        assert!(
            t.hook("B", "Bash", json!({"command": steal}))
                .unwrap()
                .contains("another live agent"),
            "{steal}"
        );
    }
    assert_eq!(
        t.hook(
            "B",
            "Bash",
            json!({"command": "git mux commit --take -m 'area: x' -- a.txt"})
        ),
        None
    );

    t.mux(&["commit", "-m", "area: a", "--", "a.txt"]);
    assert_eq!(t.hook("B", "Edit", json!({"file_path": a})), None);
}

#[test]
fn shell_guard() {
    let t = T::new();
    let cases = [
        ("git stash list", false),
        ("git add -A", true),
        ("git add a.txt", false),
        ("git checkout -- a.txt", false),
        ("git checkout feature", true),
        ("git reset --hard", true),
        ("git reset -- a.txt", false),
        ("git pull --rebase", true),
        ("git status\ngit clean -fd", true),
        ("git -C /tmp clean -fd", false),
        ("cat <<'EOF' > note\ngit clean -fd\nEOF\ngit status", false),
        ("echo 'git clean -fd'", false),
        ("A=1 git rebase main", true),
        ("git mux commit -m 'area: x' -- a.txt", false),
    ];
    for (command, blocked) in cases {
        assert_eq!(
            t.hook("A", "Bash", json!({"command": command})).is_some(),
            blocked,
            "{command}"
        );
    }
    let elsewhere = json!({"session_id": "A", "cwd": "/", "tool_name": "Bash", "hook_event_name": "PreToolUse",
                           "tool_input": {"command": format!("cd {} && git clean -fd", t.repo.display())}});
    assert_eq!(t.hook_raw(&elsewhere.to_string()).status.code(), Some(2));
}

#[test]
fn the_hook_fails_closed_and_ignores_other_checkouts() {
    let t = T::new();
    let garbled = format!("{{\"cwd\": \"{}\", ", t.repo.display());
    let o = t.hook_raw(&garbled);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("guard failed"));
    let other = json!({"session_id": "A", "cwd": "/elsewhere", "tool_name": "Bash", "tool_input": {"command": "git clean -fd"}});
    assert_eq!(t.hook_raw(&other.to_string()).status.code(), Some(0));
}

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
fn git_itself_refuses_commit_stash_and_push_in_the_shared_checkout() {
    let t = T::new();
    let head = t.rev(&t.repo, "HEAD");
    t.write("b.txt", "another session's unsaved edit\n");
    t.write("a.txt", "mine\n");
    let ahead = t.sh(
        &t.repo,
        &["git", "commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "x"],
    );
    let target = t.tmp.to_string_lossy().into_owned();
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
fn a_gone_session_frees_its_files() {
    let t = T::new();
    let a = t.repo.join("a.txt").to_string_lossy().into_owned();
    assert_eq!(t.hook("A", "Edit", json!({"file_path": a})), None);
    t.write("a.txt", "A is editing\n");
    assert!(
        t.hook("B", "Edit", json!({"file_path": a}))
            .unwrap()
            .contains("another live agent")
    );
    let mut gone = Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let mut held = t.leases();
    held["a.txt"]["pid"] = json!(gone.id());
    held["a.txt"]["start"] = json!("Thu Jan  1 00:00:00 1970");
    fs::write(t.state().join("leases.json"), held.to_string()).unwrap();
    assert_eq!(t.hook("B", "Edit", json!({"file_path": a})), None);
    assert_eq!(t.leases()["a.txt"]["session"], "B");
}

#[test]
fn shell_writes_get_an_owner() {
    let t = T::new();
    let write = json!({"command": "echo new > c.txt"});
    assert_eq!(
        t.hook_at("A", "Bash", write.clone(), "PreToolUse", "1"),
        None
    );
    t.write("c.txt", "new\n");
    assert_eq!(
        t.hook_at("A", "Bash", write.clone(), "PostToolUse", "1"),
        None
    );
    assert_eq!(t.leases()["c.txt"]["session"], "A");

    assert_eq!(
        t.hook_at("B", "Bash", write.clone(), "PreToolUse", "2"),
        None
    );
    t.write("c.txt", "overwritten by B\n");
    assert!(
        t.hook_at("B", "Bash", write, "PostToolUse", "2")
            .unwrap()
            .contains("c.txt (session A)")
    );
    let c = t.repo.join("c.txt").to_string_lossy().into_owned();
    assert!(
        t.hook("B", "Edit", json!({"file_path": c}))
            .unwrap()
            .contains("another live agent")
    );
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

#[test]
fn overlapping_commands_leave_a_change_unowned() {
    let t = T::new();
    let run = json!({"command": "make check"});
    fs::create_dir_all(t.state().join("shell")).unwrap();
    fs::write(
        t.state().join("shell/old-format.json"),
        r#"{"t": {"a.txt": null}}"#,
    )
    .unwrap();
    fs::write(t.state().join("shell/torn.json"), "{").unwrap();
    assert_eq!(t.hook_at("A", "Bash", run.clone(), "PreToolUse", "a"), None);
    assert_eq!(t.hook_at("B", "Bash", run.clone(), "PreToolUse", "b"), None);
    t.write("c.txt", "written by A or B\n");
    assert_eq!(
        t.hook_at("B", "Bash", run.clone(), "PostToolUse", "b"),
        None
    );
    assert_eq!(
        t.hook_at("A", "Bash", run.clone(), "PostToolUse", "a"),
        None
    );
    assert!(t.leases().get("c.txt").is_none());

    assert_eq!(
        t.hook_at("A", "Bash", run.clone(), "PreToolUse", "a2"),
        None
    );
    let b = t.repo.join("b.txt").to_string_lossy().into_owned();
    assert_eq!(t.hook("B", "Edit", json!({"file_path": b})), None);
    t.write("b.txt", "B's edit during A's command\n");
    assert_eq!(t.hook_at("A", "Bash", run, "PostToolUse", "a2"), None);
}

#[test]
fn verification_slots_are_locked_against_cleanup() {
    let t = T::new();
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: a2", "--", "a.txt"]);
    let slot = t.tmp.join("slots/mux-verify-0");
    assert!(!t.git_ok(&t.repo, &["worktree", "remove", &slot.to_string_lossy()]));
    assert!(slot.join(".git").exists());
}

#[test]
fn install_wires_each_agent_once_and_keeps_other_hooks() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    let theirs = json!({"model": "x", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
        {"type": "command", "command": "/opt/other-guard"},
        {"type": "command", "command": "/old/.git/mux/bin/hook"}]}]}});
    fs::write(home.join(".claude/settings.json"), theirs.to_string()).unwrap();
    for _ in 0..2 {
        t.mux(&["install"]);
    }
    let claude: Value =
        serde_json::from_slice(&fs::read(home.join(".claude/settings.json")).unwrap()).unwrap();
    assert_eq!(claude.as_object().unwrap().keys().next().unwrap(), "model");
    let pre = claude["hooks"]["PreToolUse"].as_array().unwrap();
    let commands: Vec<&str> = pre
        .iter()
        .flat_map(|g| g["hooks"].as_array().unwrap())
        .map(|h| h["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0], "/opt/other-guard");
    assert!(commands[1].ends_with("git-mux' hook"));
    let codex: Value =
        serde_json::from_slice(&fs::read(home.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(codex["hooks"]["PostToolUse"][0]["matcher"], "^Bash$");
    let registry = fs::read_to_string(t.tmp.join("config/gitmux/repos")).unwrap();
    assert_eq!(registry, format!("{}\n", t.repo.display()));
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
