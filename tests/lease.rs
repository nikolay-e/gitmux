mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The binary run as `session`, whatever agent process this suite happens to run under.
fn as_session(t: &T, session: &str, args: &[&str]) -> Output {
    t.run(args, &[("CLAUDE_CODE_SESSION_ID", Path::new(session))])
}

#[test]
fn leases_keep_sessions_off_each_others_files() {
    let t = T::new();
    agents(&t, "claude");
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

    let o = as_session(&t, "A", &["commit", "-m", "area: a", "--", "a.txt"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(t.hook("B", "Edit", json!({"file_path": a})), None);
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

/// One hook call as a session's main thread (`agent` None) or one of its subagents.
fn call(
    t: &T,
    session: &str,
    agent: Option<&str>,
    tool: &str,
    input: Value,
    event: &str,
) -> Output {
    let mut payload = json!({"session_id": session, "cwd": t.repo, "tool_name": tool,
                             "tool_input": input, "hook_event_name": event, "tool_use_id": "t"});
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
        payload["agent_type"] = json!("general-purpose");
    }
    t.hook_raw(&payload.to_string())
}

fn edit(t: &T, session: &str, agent: Option<&str>, file: &str) -> bool {
    let path = t.repo.join(file);
    let o = call(
        t,
        session,
        agent,
        "Edit",
        json!({"file_path": path}),
        "PreToolUse",
    );
    match o.status.code() {
        Some(2) => {
            assert!(String::from_utf8_lossy(&o.stderr).contains("another live agent"));
            false
        }
        _ => {
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            true
        }
    }
}

fn end(t: &T, session: &str, agent: Option<&str>, event: &str) {
    let o = call(t, session, agent, "", json!({}), event);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn two_subagents_of_one_session_hold_leases_against_each_other() {
    let t = T::new();
    assert!(edit(&t, "S", Some("one"), "a.txt"));
    t.write("a.txt", "one is editing\n");
    assert_eq!(t.leases()["a.txt"]["agent"], "one");
    assert!(!edit(&t, "S", Some("two"), "a.txt"));
    assert!(edit(&t, "S", Some("one"), "a.txt"));
    assert!(
        edit(&t, "S", None, "a.txt"),
        "the main thread is not foreign"
    );
    assert!(!edit(&t, "T", None, "a.txt"));

    end(&t, "S", Some("one"), "SubagentStop");
    let held = t.leases();
    assert_eq!(held["a.txt"]["session"], "S");
    assert!(held["a.txt"].get("agent").is_none(), "{held}");
    assert!(edit(&t, "S", Some("two"), "a.txt"));
    assert!(!edit(&t, "T", None, "a.txt"));
}

fn lstart(pid: u32) -> String {
    let o = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Marks `file`'s lease as held by this test process, which stays alive for the whole test.
fn held_by_this_process(t: &T, file: &str) {
    let mut held = t.leases();
    held[file]["pid"] = json!(std::process::id());
    held[file]["start"] = json!(lstart(std::process::id()));
    fs::write(t.state().join("leases.json"), held.to_string()).unwrap();
}

#[test]
fn the_binary_refuses_a_file_another_live_session_holds() {
    let t = T::new();
    agents(&t, "claude");
    for file in ["a.txt", "b.txt"] {
        assert!(edit(&t, "A", None, file));
        t.write(file, "A is editing\n");
        held_by_this_process(&t, file);
    }
    let before = t.rev(&t.repo, "HEAD");
    for session in ["B", ""] {
        let env: &[(&str, &Path)] = if session.is_empty() {
            &[]
        } else {
            &[("CLAUDE_CODE_SESSION_ID", Path::new(session))]
        };
        let o = t.run(&["commit", "-m", "area: steal", "--", "a.txt"], env);
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!o.status.success(), "{session:?}: {err}");
        assert!(err.contains("a.txt") && err.contains("--take"), "{err}");
        assert_eq!(t.rev(&t.repo, "HEAD"), before);
    }

    let id = Path::new("B");
    let o = t.run(
        &["commit", "--take", "-m", "area: take", "--", "a.txt"],
        &[("CLAUDE_CODE_SESSION_ID", id)],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(t.head_file("a.txt"), "A is editing\n");

    let o = t.run(
        &["commit", "-m", "area: own", "--", "b.txt"],
        &[("CLAUDE_CODE_SESSION_ID", Path::new("A"))],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(t.head_file("b.txt"), "A is editing\n");
}

#[test]
fn session_end_releases_the_sessions_leases() {
    let t = T::new();
    for (session, file) in [("A", "a.txt"), ("A", "b.txt"), ("B", "x.txt")] {
        assert!(edit(&t, session, None, file));
        t.write(file, "editing\n");
    }
    end(&t, "A", None, "SessionEnd");
    let held = t.leases();
    assert!(
        held.get("a.txt").is_none() && held.get("b.txt").is_none(),
        "{held}"
    );
    assert_eq!(held["x.txt"]["session"], "B");
    assert!(edit(&t, "B", None, "a.txt"));
}

/// Makes this test process the only agent process the binary recognizes, so that a caller with
/// no session is identified by it whether or not the suite runs under a real agent.
fn this_process_is_the_agent(t: &T) {
    let o = Command::new("ps")
        .args(["-o", "comm=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let comm = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let name = comm.rsplit('/').next().unwrap();
    t.sh(&t.repo, &["git", "config", "gitmux.agent", name]);
}

#[test]
fn a_caller_without_a_session_is_judged_by_agent_process_alone() {
    let t = T::new();
    this_process_is_the_agent(&t);
    for file in ["a.txt", "b.txt"] {
        assert!(edit(&t, "A", None, file));
        t.write(file, "A is editing\n");
    }
    held_by_this_process(&t, "a.txt");
    let mut sleeper = Command::new("sleep").arg("60").spawn().unwrap();
    let mut held = t.leases();
    held["b.txt"]["pid"] = json!(sleeper.id());
    held["b.txt"]["start"] = json!(lstart(sleeper.id()));
    fs::write(t.state().join("leases.json"), held.to_string()).unwrap();

    let other = t.run(&["commit", "-m", "area: other", "--", "b.txt"], &[]);
    let err = String::from_utf8_lossy(&other.stderr).into_owned();
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert!(!other.status.success(), "{err}");
    assert!(err.contains("b.txt") && err.contains("--take"), "{err}");

    let own = t.run(&["commit", "-m", "area: own", "--", "a.txt"], &[]);
    assert!(
        own.status.success(),
        "{}",
        String::from_utf8_lossy(&own.stderr)
    );
    assert_eq!(t.head_file("a.txt"), "A is editing\n");
}

/// A `node` that is a shell and a script named like an agent: the shape of `node …/bin/gemini`,
/// with the node process as the one that stays alive. Every call gets its own directory, so a
/// second fake never rewrites a script a running one is still reading.
fn fake_node(t: &T, script: &str, body: &str) -> Command {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let dir = t
        .tmp
        .join("fake")
        .join(CALLS.fetch_add(1, Ordering::Relaxed).to_string());
    fs::create_dir_all(&dir).unwrap();
    let node = dir.join("node");
    let shell = ["/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|s| Path::new(s).exists())
        .unwrap();
    std::os::unix::fs::symlink(shell, &node).unwrap();
    let file = dir.join(script);
    fs::write(&file, format!("#!/bin/sh\n{body}")).unwrap();
    let mut c = cmd(&node.to_string_lossy(), &t.tmp);
    c.arg(&file)
        .env("BIN", BIN)
        .env("PAYLOAD", t.tmp.join("payload.json"))
        .current_dir("/");
    c
}

fn edit_payload(t: &T, session: &str, file: &str) {
    let payload = json!({"session_id": session, "cwd": t.repo, "tool_name": "Edit",
                         "tool_input": {"file_path": t.repo.join(file)},
                         "hook_event_name": "PreToolUse", "tool_use_id": "t"});
    fs::write(t.tmp.join("payload.json"), payload.to_string()).unwrap();
}

fn agents(t: &T, names: &str) {
    t.sh(&t.repo, &["git", "config", "gitmux.agent", names]);
}

fn commit_under(t: &T, script: &str, session: Option<&str>, file: &str) -> Output {
    let mut c = fake_node(
        t,
        script,
        &format!(
            "cd '{}'\n\"$BIN\" commit -m 'area: c' -- {file}\n",
            t.repo.display()
        ),
    );
    if let Some(session) = session {
        c.env("CLAUDE_CODE_SESSION_ID", session);
    }
    c.output().unwrap()
}

#[test]
fn a_node_running_an_agents_script_is_that_agent() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let t = T::new();
    agents(&t, "gemini");
    edit_payload(&t, "G", "a.txt");
    t.write("a.txt", "gemini is editing\n");
    let mut first = fake_node(
        &t,
        "gemini",
        &format!(
            "\"$BIN\" hook < \"$PAYLOAD\" > /dev/null || exit 1\necho ready\nread go\ncd '{}'\n\"$BIN\" commit -m 'area: g' -- a.txt > /dev/null 2>&1\necho done $?\n",
            t.repo.display()
        ),
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let mut lines = BufReader::new(first.stdout.take().unwrap()).lines();
    assert_eq!(lines.next().unwrap().unwrap(), "ready");

    let held = t.leases();
    assert_eq!(held["a.txt"]["pid"], json!(first.id()), "{held}");
    assert_eq!(held["a.txt"]["start"], json!(lstart(first.id())));

    let other = commit_under(&t, "gemini", None, "a.txt");
    let err = String::from_utf8_lossy(&other.stderr).into_owned();
    assert!(!other.status.success(), "{err}");
    assert!(err.contains("a.txt") && err.contains("--take"), "{err}");

    writeln!(first.stdin.take().unwrap(), "go").unwrap();
    assert_eq!(lines.next().unwrap().unwrap(), "done 0");
    first.wait().unwrap();
    assert_eq!(t.head_file("a.txt"), "gemini is editing\n");
}

#[test]
fn a_node_running_another_script_is_no_agent() {
    let t = T::new();
    agents(&t, "gemini");
    edit_payload(&t, "V", "a.txt");
    let o = fake_node(&t, "vite", "\"$BIN\" hook < \"$PAYLOAD\" > /dev/null\n")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let held = t.leases();
    assert_eq!(held["a.txt"]["session"], "V", "{held}");
    assert!(held["a.txt"].get("pid").is_none(), "{held}");
}

#[test]
fn the_session_variable_counts_only_for_claude_or_without_an_agent() {
    let t = T::new();
    agents(&t, "gemini");
    for file in ["a.txt", "b.txt"] {
        assert_eq!(
            t.hook("X", "Edit", json!({"file_path": t.repo.join(file)})),
            None
        );
        t.write(file, "X is editing\n");
    }
    let o = t.run(
        &["commit", "-m", "area: own", "--", "a.txt"],
        &[("CLAUDE_CODE_SESSION_ID", Path::new("X"))],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let o = commit_under(&t, "gemini", Some("X"), "b.txt");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(!o.status.success(), "{err}");
    assert!(err.contains("b.txt") && err.contains("--take"), "{err}");

    agents(&t, "claude");
    t.sh(
        &t.repo,
        &["git", "config", "--add", "gitmux.agent", "gemini"],
    );
    let o = commit_under(&t, "claude", Some("X"), "b.txt");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
