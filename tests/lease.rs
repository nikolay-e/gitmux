mod common;

use common::*;
use serde_json::json;
use std::fs;
use std::process::Command;

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
