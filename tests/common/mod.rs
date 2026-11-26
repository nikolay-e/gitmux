//! The harness: real repositories, real git hooks and real concurrent processes against the
//! git-mux binary.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const BIN: &str = env!("CARGO_BIN_EXE_git-mux");

pub const PRE_COMMIT: &str = r#"#!/bin/sh
files=$(git diff --cached --name-only --diff-filter=ACMR -- '*.txt')
[ -n "$files" ] || exit 0
if grep -l BAD $files; then exit 1; fi
perl -pi -e 's/fixme/fixed/g' $files
git diff --quiet -- $files || exit 1
if grep -rl --include='*.txt' BROKEN .; then exit 1; fi
"#;
pub const COMMIT_MSG: &str = "#!/bin/sh\ngrep -qE '^[a-z]+: ' \"$1\"\n";
pub const PRE_PUSH: &str = "#!/bin/sh\ncat >/dev/null\n! git grep -q NOPUSH HEAD -- '*.txt' && git rev-parse --show-toplevel > \"$MUX_PROBE\"\n";

pub struct T {
    pub tmp: PathBuf,
    pub repo: PathBuf,
}

pub static N: AtomicUsize = AtomicUsize::new(0);

pub fn cmd(program: &str, tmp: &Path) -> Command {
    let mut c = Command::new(program);
    for (k, _) in std::env::vars() {
        if k.starts_with("GIT_")
            || k.starts_with("CLAUDE")
            || k.starts_with("CODEX")
            || k.starts_with("GEMINI")
        {
            c.env_remove(k);
        }
    }
    c.env("HOME", tmp.join("home"))
        .env("XDG_CONFIG_HOME", tmp.join("config"))
        .env("XDG_CACHE_HOME", tmp.join("cache"))
        .env("GIT_CONFIG_NOSYSTEM", "1");
    c
}

pub fn configure(t: &T, dir: &Path) {
    for (k, v) in [
        ("user.name", "t"),
        ("user.email", "t@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        t.sh(dir, &["git", "config", k, v]);
    }
}

impl T {
    pub fn new() -> T {
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
        t.mux(&["install"]);
        t
    }

    pub fn sh(&self, dir: &Path, args: &[&str]) -> String {
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

    pub fn git_ok(&self, dir: &Path, args: &[&str]) -> bool {
        cmd("git", &self.tmp)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success()
    }

    pub fn run(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut c = cmd(BIN, &self.tmp);
        for (k, v) in env {
            c.env(k, v);
        }
        c.args(args).current_dir(&self.repo).output().unwrap()
    }

    pub fn mux(&self, args: &[&str]) -> Output {
        let o = self.run(args, &[]);
        assert!(
            o.status.success(),
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }

    pub fn write(&self, path: &str, text: &str) {
        fs::write(self.repo.join(path), text).unwrap();
    }

    pub fn read(&self, path: &str) -> String {
        fs::read_to_string(self.repo.join(path)).unwrap()
    }

    pub fn head_file(&self, path: &str) -> String {
        self.sh(&self.repo, &["git", "show", &format!("HEAD:{path}")])
    }

    pub fn rev(&self, dir: &Path, rev: &str) -> String {
        self.sh(dir, &["git", "rev-parse", rev]).trim().to_string()
    }

    pub fn state(&self) -> PathBuf {
        self.repo.join(".git/gitmux")
    }

    pub fn leases(&self) -> Value {
        fs::read(self.state().join("leases.json"))
            .map(|d| serde_json::from_slice(&d).unwrap())
            .unwrap_or(json!({}))
    }

    /// The denial's stderr, the PostToolUse note, or None when the call passes silently.
    pub fn hook_at(
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

    pub fn hook(&self, session: &str, tool: &str, input: Value) -> Option<String> {
        self.hook_at(session, tool, input, "PreToolUse", "t")
    }

    pub fn hook_raw(&self, payload: &str) -> Output {
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

    pub fn with_remote(&self) -> PathBuf {
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

    pub fn upstream_commit(&self, remote: &Path, edits: &[(&str, Option<&str>)], message: &str) {
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
