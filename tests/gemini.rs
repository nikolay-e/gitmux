mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

fn gemini(t: &T, payload: &Value) -> Output {
    let mut child = cmd(BIN, &t.tmp)
        .args(["hook", "gemini"])
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
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn event(t: &T, session: &str, name: &str, extra: Value) -> Output {
    let mut p = json!({"session_id": session, "cwd": t.repo, "hook_event_name": name,
                       "transcript_path": "/dev/null", "timestamp": "2026-11-26T00:00:00Z"});
    for (k, v) in extra.as_object().unwrap() {
        p[k.as_str()] = v.clone();
    }
    gemini(t, &p)
}

fn tool(t: &T, session: &str, name: &str, tool: &str, input: Value) -> Output {
    let mut extra = json!({"tool_name": tool, "tool_input": input});
    if name == "AfterTool" {
        extra["tool_response"] = json!({"llmContent": "done", "returnDisplay": "done"});
    }
    event(t, session, name, extra)
}

fn shell(t: &T, session: &str, name: &str, command: &str) -> Output {
    tool(
        t,
        session,
        name,
        "run_shell_command",
        json!({"command": command}),
    )
}

fn denied(o: &Output) -> Option<String> {
    (o.status.code() == Some(2)).then(|| String::from_utf8_lossy(&o.stderr).into_owned())
}

fn passes(o: &Output) -> bool {
    o.status.success() && o.stdout.is_empty()
}

fn pending(t: &T, session: &str) -> Vec<String> {
    fs::read(t.state().join(format!("shell/{session}.json")))
        .map(|d| {
            let log: Value = serde_json::from_slice(&d).unwrap();
            log["calls"].as_object().unwrap().keys().cloned().collect()
        })
        .unwrap_or_default()
}

fn context(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn path_of(t: &T, file: &str) -> String {
    t.repo.join(file).to_string_lossy().into_owned()
}

fn link(t: &T, at: &Path) {
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            &at.to_string_lossy(),
            "-b",
            "wt",
        ],
    );
}

#[test]
fn a_plain_commit_is_refused_and_the_message_names_git_mux_commit() {
    let t = T::new();
    let reason = denied(&shell(&t, "G", "BeforeTool", "git commit -am x")).unwrap();
    assert!(reason.contains("git mux commit"), "{reason}");
    assert!(passes(&shell(&t, "G", "BeforeTool", "git status")));
}

#[test]
fn dir_path_decides_where_the_command_runs() {
    let t = T::new();
    let wt = t.tmp.join("linked");
    link(&t, &wt);
    let inside = t.repo.join(".claude/worktrees/x");
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            &inside.to_string_lossy(),
            "-b",
            "wt2",
        ],
    );
    let clean = |dir: Option<&Path>| {
        let mut input = json!({"command": "git clean -fdx"});
        if let Some(d) = dir {
            input["dir_path"] = json!(d);
        }
        tool(&t, "G", "BeforeTool", "run_shell_command", input)
    };
    assert!(passes(&clean(Some(&wt))));
    assert!(passes(&clean(Some(Path::new(".claude/worktrees/x")))));
    let reason = denied(&clean(Some(Path::new(".")))).unwrap();
    assert!(reason.contains("shared checkout"), "{reason}");
    assert!(
        denied(&clean(Some(Path::new(".claude/worktrees/.."))))
            .unwrap()
            .contains("shared checkout")
    );
    assert!(denied(&clean(Some(&t.repo))).is_some());
    assert!(denied(&clean(None)).is_some());
}

#[test]
fn file_tools_take_and_respect_leases() {
    let t = T::new();
    assert_eq!(
        t.hook("B", "Edit", json!({"file_path": path_of(&t, "a.txt")})),
        None
    );
    t.write("a.txt", "B is editing\n");
    for (name, input) in [
        (
            "write_file",
            json!({"file_path": path_of(&t, "a.txt"), "content": "x"}),
        ),
        (
            "replace",
            json!({"file_path": "a.txt", "old_string": "a", "new_string": "b"}),
        ),
    ] {
        let reason = denied(&tool(&t, "gem-G", "BeforeTool", name, input)).unwrap();
        assert!(reason.contains("another live agent"), "{name}: {reason}");
    }
    assert!(passes(&tool(
        &t,
        "gem-G",
        "BeforeTool",
        "write_file",
        json!({"file_path": path_of(&t, "b.txt"), "content": "x"})
    )));
    t.write("b.txt", "gemini wrote\n");
    let status = String::from_utf8_lossy(&t.mux(&["status"]).stdout).into_owned();
    assert!(status.contains("b.txt\tgem-G"), "{status}");
    assert!(status.contains("a.txt\tB"), "{status}");
}

#[test]
fn a_shell_command_is_matched_across_before_and_after_tool_without_a_call_id() {
    let t = T::new();
    let write = "echo new > c.txt";
    assert!(passes(&shell(&t, "gem-A", "BeforeTool", write)));
    let keys = pending(&t, "gem-A");
    assert_eq!(keys.len(), 1);
    assert!(keys[0].starts_with("gemini-"), "{keys:?}");
    t.write("c.txt", "new\n");
    assert!(passes(&shell(&t, "gem-A", "AfterTool", write)));
    assert!(pending(&t, "gem-A").is_empty());
    assert_eq!(t.leases()["c.txt"]["session"], "gem-A");

    assert!(passes(&shell(&t, "gem-B", "BeforeTool", write)));
    t.write("c.txt", "overwritten by B\n");
    let out = context(&shell(&t, "gem-B", "AfterTool", write));
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "AfterTool");
    let text = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("c.txt (session gem-A)"), "{text}");
}

#[test]
fn session_start_reaches_the_model_as_additional_context() {
    let t = T::new();
    let out = context(&event(
        &t,
        "G",
        "SessionStart",
        json!({"source": "startup"}),
    ));
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let text = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("git mux commit -m MSG -- PATH"), "{text}");
    let elsewhere = event(&t, "G", "SessionStart", json!({"cwd": "/"}));
    assert!(passes(&elsewhere));
}

#[test]
fn after_agent_drops_pending_calls_and_session_end_releases_leases() {
    let t = T::new();
    assert!(passes(&shell(&t, "gem-G", "BeforeTool", "make one")));
    assert!(passes(&shell(&t, "gem-G", "BeforeTool", "make two")));
    assert_eq!(pending(&t, "gem-G").len(), 2);
    assert!(passes(&event(
        &t,
        "gem-G",
        "AfterAgent",
        json!({"prompt": "p", "prompt_response": "r", "stop_hook_active": false})
    )));
    assert!(pending(&t, "gem-G").is_empty());

    let held = json!({"file_path": path_of(&t, "a.txt"), "content": "x"});
    assert!(passes(&tool(
        &t,
        "gem-G",
        "BeforeTool",
        "write_file",
        held.clone()
    )));
    t.write("a.txt", "G is editing\n");
    assert!(denied(&tool(&t, "gem-H", "BeforeTool", "write_file", held.clone())).is_some());
    assert!(passes(&event(
        &t,
        "gem-G",
        "SessionEnd",
        json!({"reason": "exit"})
    )));
    assert!(!t.state().join("shell/gem-G.json").exists());
    assert!(passes(&tool(&t, "gem-H", "BeforeTool", "write_file", held)));
}

#[test]
fn other_tools_and_events_pass_in_silence() {
    let t = T::new();
    let read = json!({"file_path": path_of(&t, "a.txt")});
    assert!(passes(&tool(&t, "G", "BeforeTool", "read_file", read)));
    assert!(passes(&tool(
        &t,
        "G",
        "BeforeTool",
        "mcp_x_clean",
        json!({"command": "git clean -fd"})
    )));
    for name in ["BeforeModel", "AfterModel", "PreCompress", "Notification"] {
        assert!(passes(&event(&t, "G", name, json!({}))), "{name}");
    }
}

#[test]
fn only_a_before_tool_failure_refuses_and_the_other_events_report_and_go_on() {
    let t = T::new();
    assert!(passes(&shell(&t, "gem-G", "BeforeTool", "make")));
    let lock = t.state().join("shell.lock");
    let _ = fs::remove_file(&lock);
    fs::create_dir(&lock).unwrap();

    for (name, o) in [
        ("AfterTool", shell(&t, "gem-G", "AfterTool", "make")),
        ("AfterAgent", event(&t, "gem-G", "AfterAgent", json!({}))),
        ("SessionEnd", event(&t, "gem-G", "SessionEnd", json!({}))),
    ] {
        assert_eq!(o.status.code(), Some(0), "{name}");
        assert!(o.stdout.is_empty(), "{name}");
        let said = String::from_utf8_lossy(&o.stderr);
        assert!(
            said.contains(&format!("{name} hook failed")),
            "{name}: {said}"
        );
    }
    let reason = denied(&shell(&t, "gem-G", "BeforeTool", "make")).unwrap();
    assert!(reason.contains("guard failed"), "{reason}");
}
