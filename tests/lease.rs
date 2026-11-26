mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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
