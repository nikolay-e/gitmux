mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Output;

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

fn payload(t: &T, session: &str, event: &str, extra: Value) -> Value {
    let mut p = json!({"session_id": session, "cwd": t.repo, "hook_event_name": event});
    for (k, v) in extra.as_object().unwrap() {
        p[k.as_str()] = v.clone();
    }
    p
}

fn fire(t: &T, session: &str, event: &str, extra: Value) -> Output {
    t.hook_raw(&payload(t, session, event, extra).to_string())
}

fn bash(t: &T, session: &str, event: &str, id: &str, cwd: &Path, command: Value) -> Output {
    fire(
        t,
        session,
        event,
        json!({"cwd": cwd, "tool_name": "Bash", "tool_use_id": id, "tool_input": {"command": command}}),
    )
}

fn denied(o: &Output) -> Option<String> {
    (o.status.code() == Some(2)).then(|| String::from_utf8_lossy(&o.stderr).into_owned())
}

fn passes(o: &Output) -> bool {
    o.status.success() && o.stdout.is_empty()
}

fn pending(t: &T, session: &str) -> Value {
    fs::read(t.state().join(format!("shell/{session}.json")))
        .map(|d| serde_json::from_slice::<Value>(&d).unwrap()["calls"].clone())
        .unwrap_or(json!({}))
}

fn path_of(t: &T, file: &str) -> String {
    t.repo.join(file).to_string_lossy().into_owned()
}

#[test]
fn a_garbled_payload_is_refused_only_when_it_names_an_enrolled_root() {
    let t = T::new();
    let named = format!("{{\"cwd\": \"{}\" nope", t.repo.display());
    assert_eq!(t.hook_raw(&named).status.code(), Some(2));
    assert_eq!(
        t.hook_raw("{\"cwd\": \"/elsewhere\" nope").status.code(),
        Some(0)
    );
}

#[test]
fn a_deleted_enrolled_root_blocks_nothing() {
    let t = T::new();
    let named = format!("{{\"cwd\": \"{}\" nope", t.repo.display());
    fs::remove_dir_all(&t.repo).unwrap();
    assert_eq!(t.hook_raw(&named).status.code(), Some(0));
    let clean = bash(&t, "A", "PreToolUse", "1", &t.repo, json!("git clean -fd"));
    assert!(passes(&clean));
}

#[test]
fn linked_worktrees_and_nested_repositories_under_the_root_are_free() {
    let t = T::new();
    let wt = t.repo.join(".claude/worktrees/x");
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            &wt.to_string_lossy(),
            "-b",
            "wt",
        ],
    );
    let nested = t.repo.join("nested");
    fs::create_dir(&nested).unwrap();
    t.sh(&nested, &["git", "init", "-q"]);
    fs::write(nested.join("n.txt"), "n\n").unwrap();

    let clean = json!("git clean -fd");
    let reason = denied(&bash(&t, "A", "PreToolUse", "1", &t.repo, clean.clone())).unwrap();
    assert!(reason.contains("shared checkout"), "{reason}");
    for free in [&wt, &nested] {
        assert!(passes(&bash(
            &t,
            "A",
            "PreToolUse",
            "2",
            free,
            clean.clone()
        )));
    }
    let from_root = format!("git -C {} clean -fd", wt.display());
    assert!(passes(&bash(
        &t,
        "A",
        "PreToolUse",
        "3",
        &t.repo,
        json!(from_root)
    )));

    for file in [wt.join("a.txt"), nested.join("n.txt")] {
        let input = json!({"file_path": file});
        assert_eq!(t.hook("A", "Edit", input.clone()), None);
        fs::write(&file, "edited\n").unwrap();
        assert_eq!(t.hook("B", "Edit", input), None, "{}", file.display());
    }
    assert_eq!(t.leases(), json!({}));
}

#[test]
fn an_edit_through_a_symlinked_path_takes_the_lease() {
    let t = T::new();
    let dir = t.tmp.join("link");
    std::os::unix::fs::symlink(&t.repo, &dir).unwrap();
    let file = t.tmp.join("file-link");
    std::os::unix::fs::symlink(t.repo.join("b.txt"), &file).unwrap();

    assert_eq!(
        t.hook("A", "Edit", json!({"file_path": dir.join("a.txt")})),
        None
    );
    t.write("a.txt", "A is editing\n");
    assert_eq!(t.hook("A", "Edit", json!({"file_path": file})), None);
    t.write("b.txt", "A is editing\n");
    assert_eq!(t.leases()["a.txt"]["session"], "A");
    assert_eq!(t.leases()["b.txt"]["session"], "A");
    for name in ["a.txt", "b.txt"] {
        let held = t.hook("B", "Edit", json!({"file_path": path_of(&t, name)}));
        assert!(held.unwrap().contains("another live agent"), "{name}");
    }
}

#[test]
fn an_unknown_directory_counts_as_the_root_the_shell_is_in_or_names() {
    let t = T::new();
    let denial =
        |cwd: &Path, command: &str| denied(&bash(&t, "A", "PreToolUse", "1", cwd, json!(command)));
    let reason = denial(&t.repo, "cd $X && git clean -fd").unwrap();
    assert!(reason.contains("shared checkout"), "{reason}");
    assert_eq!(denial(&t.repo, "cd $X && git status"), None);

    let outside = Path::new("/");
    let naming = format!("cd $X && git clean -fd; ls {}", t.repo.display());
    assert!(denial(outside, &naming).is_some());
    assert_eq!(denial(outside, "cd $X && git clean -fd"), None);
}

#[test]
fn a_cd_that_may_not_have_run_leaves_the_directory_unknown() {
    let t = T::new();
    let other = t.tmp.join("other");
    fs::create_dir(&other).unwrap();
    let wt = t.tmp.join("linked");
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            &wt.to_string_lossy(),
            "-b",
            "wt",
        ],
    );
    let (o, missing) = (other.display(), t.tmp.join("missing").display().to_string());
    let denial = |command: &str| denied(&bash(&t, "A", "PreToolUse", "1", &t.repo, json!(command)));
    let refused = [
        "git clean -fdx".to_string(),
        format!("true || cd {o}; git clean -fdx"),
        format!("if false; then cd {o}; fi; git clean -fdx"),
        format!("cd {missing}; git clean -fdx"),
        format!("(cd {o}); git clean -fdx"),
        format!("D={o} cd $D && git clean -fdx"),
        format!("cd {o} && true & git clean -fdx"),
        format!("case x in x) cd {o};; esac; git clean -fdx"),
        format!("f() {{ cd {o}; }}; git clean -fdx"),
        format!("A || cd {o} && git clean -fdx"),
        format!("false && cd {o}; git clean -fdx"),
    ];
    for command in &refused {
        let reason = denial(command).unwrap_or_else(|| panic!("allowed: {command}"));
        assert!(reason.contains("shared checkout"), "{command}: {reason}");
    }
    let reason = denial(&format!("true || cd {o}; git clean -fdx")).unwrap();
    assert!(reason.contains("can't tell which directory"), "{reason}");
    assert!(reason.contains("`git -C DIR"), "{reason}");
    let reason = denial("cd $D && git commit -m x").unwrap();
    assert!(reason.contains("can't tell which directory"), "{reason}");
    let known = denial("git clean -fdx").unwrap();
    assert!(!known.contains("can't tell"), "{known}");

    let allowed = [
        format!("cd {o} && git clean -fdx"),
        format!("cd {o}; git clean -fdx"),
        format!("cd {o}\ngit clean -fdx"),
        format!("cd {o} || exit 1; git clean -fdx"),
        format!("cd {} && git commit -m x", wt.display()),
    ];
    for command in &allowed {
        assert_eq!(denial(command), None, "{command}");
    }
}

#[test]
fn an_unparseable_command_is_refused_when_it_could_be_git_in_the_root() {
    let t = T::new();
    let go = |cwd: &Path, command: &str| bash(&t, "A", "PreToolUse", "1", cwd, json!(command));
    let reason = denied(&go(&t.repo, "git status 'unbalanced")).unwrap();
    assert!(reason.contains("cannot be parsed safely"), "{reason}");
    let reason = denied(&go(
        Path::new("/"),
        &format!("git -C {} log 'x", t.repo.display()),
    ));
    assert!(reason.unwrap().contains("cannot be parsed safely"));
    assert!(passes(&go(&t.repo, "echo 'unbalanced")));
    assert!(passes(&go(Path::new("/"), "git status 'unbalanced")));
}

#[test]
fn an_argv_array_is_judged_like_its_text() {
    let t = T::new();
    let clean = json!(["git", "clean", "-fd"]);
    assert!(denied(&bash(&t, "A", "PreToolUse", "1", &t.repo, clean.clone())).is_some());
    assert!(passes(&bash(
        &t,
        "A",
        "PreToolUse",
        "2",
        Path::new("/"),
        clean
    )));
    let status = json!(["git", "status"]);
    assert!(passes(&bash(&t, "A", "PreToolUse", "3", &t.repo, status)));
}

#[test]
fn a_revert_of_another_sessions_file_is_reported() {
    let t = T::new();
    assert_eq!(
        t.hook("A", "Edit", json!({"file_path": path_of(&t, "b.txt")})),
        None
    );
    t.write("b.txt", "A is editing\n");
    let run = json!({"command": "make clean"});
    assert_eq!(t.hook_at("B", "Bash", run.clone(), "PreToolUse", "1"), None);
    t.write("b.txt", "b\n");
    let note = t.hook_at("B", "Bash", run, "PostToolUse", "1").unwrap();
    assert!(note.contains("b.txt (session A)"), "{note}");

    assert_eq!(
        t.hook("A", "Edit", json!({"file_path": path_of(&t, "x.txt")})),
        None
    );
    t.write("x.txt", "A is editing\n");
    let run = json!({"command": "make clean"});
    assert_eq!(t.hook_at("B", "Bash", run.clone(), "PreToolUse", "2"), None);
    fs::remove_file(t.repo.join("x.txt")).unwrap();
    let note = t.hook_at("B", "Bash", run, "PostToolUse", "2").unwrap();
    assert!(note.contains("x.txt (session A)"), "{note}");
}

#[test]
fn a_failed_call_attributes_like_a_finished_one_and_each_handler_runs_once() {
    let t = T::new();
    let write = json!({"command": "echo new > c.txt"});
    assert_eq!(
        t.hook_at("A", "Bash", write.clone(), "PreToolUse", "1"),
        None
    );
    t.write("c.txt", "new\n");
    let failed = |id| {
        bash(
            &t,
            "A",
            "PostToolUseFailure",
            id,
            &t.repo,
            write["command"].clone(),
        )
    };
    assert!(passes(&failed("1")));
    assert!(passes(&failed("1")));
    assert_eq!(t.leases()["c.txt"]["session"], "A");

    assert_eq!(
        t.hook_at("B", "Bash", write.clone(), "PreToolUse", "2"),
        None
    );
    t.write("c.txt", "overwritten by B\n");
    let post = bash(
        &t,
        "B",
        "PostToolUseFailure",
        "2",
        &t.repo,
        write["command"].clone(),
    );
    let out: Value = serde_json::from_slice(&post.stdout).unwrap();
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "PostToolUseFailure"
    );
    let text = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("c.txt (session A)"), "{text}");
    let again = bash(
        &t,
        "B",
        "PostToolUseFailure",
        "2",
        &t.repo,
        write["command"].clone(),
    );
    assert!(passes(&again));
}

#[test]
fn a_pending_call_is_recorded_once_with_its_agent() {
    let t = T::new();
    let call = |id: &str, agent: Option<&str>| {
        let mut extra =
            json!({"tool_name": "Bash", "tool_use_id": id, "tool_input": {"command": "make"}});
        if let Some(a) = agent {
            extra["agent_id"] = json!(a);
        }
        assert!(passes(&fire(&t, "A", "PreToolUse", extra)));
    };
    call("m", None);
    let first = pending(&t, "A")["m"]["t0"].clone();
    call("m", None);
    call("s", Some("sub"));
    let calls = pending(&t, "A");
    assert_eq!(calls["m"]["t0"], first);
    assert!(calls["m"].get("agent_id").is_none());
    assert_eq!(calls["s"]["agent_id"], "sub");
}

#[test]
fn events_close_the_pending_calls_they_can_know_are_over() {
    let t = T::new();
    let call = |id: &str, agent: Option<&str>| {
        let mut extra =
            json!({"tool_name": "Bash", "tool_use_id": id, "tool_input": {"command": "make"}});
        if let Some(a) = agent {
            extra["agent_id"] = json!(a);
        }
        assert!(passes(&fire(&t, "A", "PreToolUse", extra)));
    };
    let keys = |t: &T| -> Vec<String> {
        let mut keys: Vec<String> = pending(t, "A")
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };

    call("denied", None);
    call("kept", None);
    let ok = fire(
        &t,
        "A",
        "PermissionDenied",
        json!({"tool_name": "Bash", "tool_use_id": "denied"}),
    );
    assert!(passes(&ok));
    assert_eq!(keys(&t), ["kept"]);
    assert!(passes(&fire(
        &t,
        "A",
        "PermissionDenied",
        json!({"tool_use_id": "denied"})
    )));

    call("sub1", Some("one"));
    call("sub2", Some("two"));
    assert!(passes(&fire(&t, "A", "Stop", json!({}))));
    assert_eq!(keys(&t), ["sub1", "sub2"]);

    assert!(passes(&fire(
        &t,
        "A",
        "SubagentStop",
        json!({"agent_id": "one"})
    )));
    assert_eq!(keys(&t), ["sub2"]);

    call("main", None);
    assert!(passes(&fire(&t, "A", "Interrupt", json!({}))));
    assert_eq!(keys(&t), ["sub2"]);

    call("failed", None);
    assert!(passes(&fire(&t, "A", "StopFailure", json!({}))));
    assert_eq!(keys(&t), ["sub2"]);

    call("other", None);
    assert!(passes(&fire(&t, "B", "Stop", json!({}))));
    assert_eq!(keys(&t), ["other", "sub2"]);

    assert!(passes(&fire(&t, "A", "SessionEnd", json!({}))));
    assert!(!t.state().join("shell/A.json").exists());
    assert!(passes(&fire(&t, "A", "SessionEnd", json!({}))));
}

#[test]
fn a_closed_call_stops_counting_as_a_running_command() {
    let t = T::new();
    let run = json!({"command": "make"});
    assert_eq!(t.hook_at("A", "Bash", run.clone(), "PreToolUse", "a"), None);
    assert_eq!(t.hook_at("B", "Bash", run.clone(), "PreToolUse", "b"), None);
    assert!(passes(&fire(&t, "A", "Stop", json!({}))));
    t.write("c.txt", "written by B\n");
    assert_eq!(t.hook_at("B", "Bash", run, "PostToolUse", "b"), None);
    assert_eq!(t.leases()["c.txt"]["session"], "B");
}

#[test]
fn session_end_releases_the_sessions_leases() {
    let t = T::new();
    assert_eq!(
        t.hook("A", "Edit", json!({"file_path": path_of(&t, "a.txt")})),
        None
    );
    t.write("a.txt", "A is editing\n");
    assert!(passes(&fire(&t, "A", "SessionEnd", json!({}))));
    assert_eq!(
        t.hook("B", "Edit", json!({"file_path": path_of(&t, "a.txt")})),
        None
    );
}

#[test]
fn session_start_explains_the_checkout_only_inside_it() {
    let t = T::new();
    let wt = t.repo.join(".claude/worktrees/x");
    t.sh(
        &t.repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            &wt.to_string_lossy(),
            "-b",
            "wt",
        ],
    );
    let said = |cwd: &Path| {
        let o = fire(&t, "A", "SessionStart", json!({"cwd": cwd}));
        assert!(o.status.success());
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let text = said(&t.repo);
    for part in [
        "shared with other agents",
        "git mux commit -m MSG -- PATH",
        "git mux push",
        "worktree",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(said(&t.repo.join("sub/dir")), text);
    assert_eq!(said(&wt), "");
    assert_eq!(said(Path::new("/")), "");
}

#[test]
fn only_a_pre_tool_use_failure_refuses_and_the_other_events_report_and_go_on() {
    let t = T::new();
    let make = |event: &str| bash(&t, "A", event, "1", &t.repo, json!("make"));
    assert!(passes(&make("PreToolUse")));
    let lock = t.state().join("shell.lock");
    let _ = fs::remove_file(&lock);
    fs::create_dir(&lock).unwrap();

    for event in ["PostToolUse", "PostToolUseFailure"] {
        let o = make(event);
        assert_eq!(o.status.code(), Some(0), "{event}");
        assert!(o.stdout.is_empty(), "{event}");
        let said = String::from_utf8_lossy(&o.stderr);
        assert!(said.contains(&format!("{event} hook failed")), "{said}");
    }
    for event in ["Stop", "StopFailure", "SessionEnd"] {
        let o = fire(&t, "A", event, json!({}));
        assert_eq!(o.status.code(), Some(0), "{event}");
        assert!(!o.stderr.is_empty(), "{event}");
    }
    let reason = denied(&make("PreToolUse")).unwrap();
    assert!(reason.contains("guard failed"), "{reason}");
}
