mod common;

use common::*;
use serde_json::json;
use std::fs;

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
