mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn json_at(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Every git-mux handler of one config: (event, matcher, handler).
fn ours(config: &Value) -> Vec<(String, Option<String>, Value)> {
    let mut found = Vec::new();
    for (event, groups) in config["hooks"].as_object().into_iter().flatten() {
        for g in groups.as_array().unwrap() {
            for h in g["hooks"].as_array().unwrap() {
                if h["command"].as_str().unwrap().contains("git-mux") {
                    found.push((
                        event.clone(),
                        g["matcher"].as_str().map(String::from),
                        h.clone(),
                    ));
                }
            }
        }
    }
    found
}

fn events(found: &[(String, Option<String>, Value)]) -> Vec<&str> {
    let mut e: Vec<&str> = found.iter().map(|(e, ..)| e.as_str()).collect();
    e.sort();
    e
}

fn mux_in(t: &T, dir: &Path, args: &[&str]) -> Output {
    let o = cmd(BIN, &t.tmp)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    o
}

fn agent_dirs(t: &T) -> PathBuf {
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    home
}

#[test]
fn install_wires_each_agent_once_and_keeps_other_hooks() {
    let t = T::new();
    let home = agent_dirs(&t);
    let theirs = json!({"model": "x", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
        {"type": "command", "command": "/opt/other-guard"},
        {"type": "command", "command": "'/old/bin/git-mux' hook", "timeout": 10}]}]}});
    fs::write(home.join(".claude/settings.json"), theirs.to_string()).unwrap();
    for _ in 0..2 {
        t.mux(&["install"]);
    }
    let claude = json_at(&home.join(".claude/settings.json"));
    assert_eq!(claude.as_object().unwrap().keys().next().unwrap(), "model");
    let found = ours(&claude);
    assert_eq!(
        events(&found),
        [
            "PermissionDenied",
            "PostToolUse",
            "PostToolUseFailure",
            "PreToolUse",
            "SessionEnd",
            "SessionStart",
            "Stop",
            "StopFailure",
            "SubagentStop"
        ]
    );
    let mut earlier = claude.clone();
    earlier["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("StopFailure");
    fs::write(home.join(".claude/settings.json"), earlier.to_string()).unwrap();
    for _ in 0..2 {
        t.mux(&["install"]);
    }
    assert_eq!(json_at(&home.join(".claude/settings.json")), claude);
    let matcher = |event: &str| found.iter().find(|(e, ..)| e == event).unwrap().1.clone();
    assert_eq!(
        matcher("PreToolUse").as_deref(),
        Some("Bash|Edit|Write|MultiEdit|NotebookEdit")
    );
    for event in ["PostToolUse", "PostToolUseFailure", "PermissionDenied"] {
        assert_eq!(matcher(event).as_deref(), Some("Bash"), "{event}");
    }
    for event in [
        "Stop",
        "StopFailure",
        "SubagentStop",
        "SessionStart",
        "SessionEnd",
    ] {
        assert_eq!(matcher(event), None, "{event}");
    }
    for (event, _, h) in &found {
        assert_eq!(h["timeout"], 30, "{event}");
        assert!(h.get("onFailure").is_none(), "{event}");
        let command = h["command"].as_str().unwrap();
        assert!(command.starts_with("[ -x '"), "{command}");
        assert!(command.ends_with("git-mux' hook"), "{command}");
    }
    let pre = claude["hooks"]["PreToolUse"].as_array().unwrap();
    let commands: Vec<&str> = pre
        .iter()
        .flat_map(|g| g["hooks"].as_array().unwrap())
        .map(|h| h["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0], "/opt/other-guard");

    let codex = json_at(&home.join(".codex/hooks.json"));
    let found = ours(&codex);
    assert_eq!(
        events(&found),
        [
            "Interrupt",
            "PostToolUse",
            "PreToolUse",
            "SessionEnd",
            "SessionStart",
            "Stop"
        ]
    );
    assert_eq!(codex["hooks"]["PostToolUse"][0]["matcher"], "^Bash$");
    assert_eq!(
        codex["hooks"]["PreToolUse"][0]["matcher"],
        "^(Bash|apply_patch|Edit|Write)$"
    );
    let registry = fs::read_to_string(t.tmp.join("config/gitmux/repos")).unwrap();
    assert_eq!(registry, format!("{}\n", t.repo.display()));
}

#[test]
fn the_wired_command_lets_the_call_through_when_the_binary_is_gone() {
    let t = T::new();
    let home = agent_dirs(&t);
    let copy = t.tmp.join("copy");
    fs::create_dir_all(&copy).unwrap();
    fs::copy(BIN, copy.join("git-mux")).unwrap();
    let o = cmd(copy.join("git-mux").to_str().unwrap(), &t.tmp)
        .arg("install")
        .current_dir(&t.repo)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let claude = json_at(&home.join(".claude/settings.json"));
    let command = ours(&claude)[0].2["command"].as_str().unwrap().to_string();
    assert!(command.contains(copy.to_str().unwrap()), "{command}");
    let run = || cmd("sh", &t.tmp).args(["-c", &command]).output().unwrap();
    assert!(
        run().status.success(),
        "the hook with its binary present, fed nothing"
    );
    fs::remove_file(copy.join("git-mux")).unwrap();
    let o = run();
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
}

#[test]
fn project_install_writes_the_exec_form_and_git_ignores_it() {
    let t = T::new();
    agent_dirs(&t);
    let file = t.repo.join(".claude/settings.local.json");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, r#"{"permissions": {"allow": ["Bash(ls)"]}}"#).unwrap();
    for _ in 0..2 {
        t.mux(&["install", "--project"]);
    }
    let config = json_at(&file);
    assert_eq!(config["permissions"]["allow"][0], "Bash(ls)");
    let found = ours(&config);
    assert_eq!(
        events(&found),
        [
            "PermissionDenied",
            "PostToolUse",
            "PostToolUseFailure",
            "PreToolUse"
        ]
    );
    for (event, matcher, h) in &found {
        assert_eq!(
            *h,
            json!({"type": "command", "command": BIN, "args": ["hook"], "onFailure": "block", "timeout": 30}),
            "{event}"
        );
        let want = if event == "PreToolUse" {
            "Bash|Edit|Write|MultiEdit|NotebookEdit"
        } else {
            "Bash"
        };
        assert_eq!(matcher.as_deref(), Some(want), "{event}");
    }
    assert!(t.git_ok(
        &t.repo,
        &["check-ignore", "-q", ".claude/settings.local.json"]
    ));
    let exclude = fs::read_to_string(t.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(
        exclude.matches("settings.local.json").count(),
        1,
        "{exclude}"
    );
    assert_eq!(t.sh(&t.repo, &["git", "status", "--porcelain"]), "");
    let global = json_at(&t.tmp.join("home/.claude/settings.json"));
    assert!(
        ours(&global)
            .iter()
            .all(|(_, _, h)| h.get("args").is_none() && h.get("onFailure").is_none())
    );
}

#[test]
fn status_in_a_fresh_repository_enrolls_nothing() {
    let t = T::new();
    let fresh = t.tmp.join("fresh");
    fs::create_dir_all(&fresh).unwrap();
    t.sh(&fresh, &["git", "init", "-q", "-b", "main"]);
    configure(&t, &fresh);
    let hooks = fresh.join(".git/hooks");
    fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
    t.sh(&fresh, &["chmod", "+x", ".git/hooks/pre-commit"]);
    mux_in(&t, &fresh, &["status"]);
    let registry = fs::read_to_string(t.tmp.join("config/gitmux/repos")).unwrap();
    assert_eq!(registry, format!("{}\n", t.repo.display()));
    assert_eq!(
        fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
    assert!(!hooks.join("reference-transaction").exists());
    assert!(!fresh.join(".git/gitmux/guard").exists());
}

#[test]
fn a_repository_with_no_commit_installs_reports_and_uninstalls() {
    let t = T::new();
    let fresh = t.tmp.join("unborn");
    fs::create_dir_all(&fresh).unwrap();
    t.sh(&fresh, &["git", "init", "-q", "-b", "main"]);
    configure(&t, &fresh);
    fs::write(fresh.join("a.txt"), "a\n").unwrap();
    mux_in(&t, &fresh, &["install"]);
    mux_in(&t, &fresh, &["status"]);
    let hooks = fresh.join(".git/hooks");
    assert!(hooks.join("reference-transaction").exists());

    let o = cmd(BIN, &t.tmp)
        .args(["commit", "-m", "t: first", "a.txt"])
        .current_dir(&fresh)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success());
    assert_eq!(
        err.trim().lines().count(),
        1,
        "one clear line, not git's bad revision: {err}"
    );
    assert!(
        err.contains("no commit yet") && !err.contains("bad revision"),
        "{err}"
    );
    assert!(!fresh.join(".git/refs/heads/main").exists());

    t.sh(&fresh, &["git", "add", "a.txt"]);
    let first = cmd("git", &t.tmp)
        .env("GIT_MUX", "1")
        .args(["commit", "-q", "--no-verify", "-m", "t: first"])
        .current_dir(&fresh)
        .status()
        .unwrap();
    assert!(first.success(), "the message's advice works");
    fs::write(fresh.join("a.txt"), "a2\n").unwrap();
    mux_in(&t, &fresh, &["commit", "-m", "t: second", "a.txt"]);
    assert_eq!(t.sh(&fresh, &["git", "rev-list", "--count", "HEAD"]), "2\n");

    mux_in(&t, &fresh, &["uninstall"]);
    assert!(!hooks.join("reference-transaction").exists());
    assert!(!fresh.join(".git/gitmux").exists());
}

#[test]
fn a_registry_entry_whose_directory_is_gone_is_pruned() {
    let t = T::new();
    let registry = t.tmp.join("config/gitmux/repos");
    let gone = t.tmp.join("gone");
    fs::write(
        &registry,
        format!("{}\n{}\n", gone.display(), t.repo.display()),
    )
    .unwrap();
    t.mux(&["status"]);
    assert_eq!(
        fs::read_to_string(&registry).unwrap(),
        format!("{}\n", t.repo.display())
    );
}

#[test]
fn uninstall_gives_every_hook_back_and_the_last_repo_takes_the_agents_along() {
    let t = T::new();
    let home = agent_dirs(&t);
    let settings = home.join(".claude/settings.json");
    fs::write(
        &settings,
        json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/opt/other"}]}]}})
            .to_string(),
    )
    .unwrap();
    let perl = "#!/usr/bin/env perl\nexit 0;\n";
    let hooks = t.repo.join(".git/hooks");
    fs::write(hooks.join("pre-push"), perl).unwrap();
    t.sh(&t.repo, &["chmod", "+x", ".git/hooks/pre-push"]);
    let local = t.repo.join(".claude/settings.local.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    fs::write(&local, r#"{"permissions": {"allow": ["Bash(ls)"]}}"#).unwrap();
    t.mux(&["install", "--project"]);
    assert!(
        fs::read_to_string(hooks.join("pre-push"))
            .unwrap()
            .contains("gitmux-chained"),
        "the perl hook is chained"
    );
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "t: a", "a.txt"]);
    let slot = t.tmp.join("slots/mux-verify-0");
    assert!(slot.exists());
    let second = t.tmp.join("second");
    fs::create_dir_all(&second).unwrap();
    t.sh(&second, &["git", "init", "-q", "-b", "main"]);
    mux_in(&t, &second, &["install"]);
    let registry = t.tmp.join("config/gitmux/repos");
    assert_eq!(fs::read_to_string(&registry).unwrap().lines().count(), 2);

    mux_in(&t, &t.repo, &["uninstall"]);
    assert_eq!(
        fs::read_to_string(hooks.join("pre-commit")).unwrap(),
        PRE_COMMIT
    );
    assert_eq!(
        fs::read_to_string(hooks.join("commit-msg")).unwrap(),
        COMMIT_MSG
    );
    assert_eq!(fs::read_to_string(hooks.join("pre-push")).unwrap(), perl);
    assert!(!hooks.join("reference-transaction").exists());
    assert!(!hooks.join("gitmux-chained").exists());
    assert!(!t.state().exists());
    assert!(!slot.exists());
    assert!(
        !t.sh(&t.repo, &["git", "worktree", "list"])
            .contains("mux-verify")
    );

    assert_eq!(
        fs::read_to_string(&registry).unwrap(),
        format!("{}\n", second.display())
    );
    assert_eq!(
        json_at(&local),
        json!({"permissions": {"allow": ["Bash(ls)"]}})
    );
    assert!(
        !ours(&json_at(&settings)).is_empty(),
        "another repo still needs the agent hooks"
    );
    t.mux(&["status"]);
    assert!(
        !hooks.join("reference-transaction").exists() && !t.state().join("guard").exists(),
        "a run in a repository that left enrolls and guards nothing"
    );

    mux_in(&t, &second, &["uninstall"]);
    assert!(!registry.exists());
    assert!(ours(&json_at(&settings)).is_empty());
    assert_eq!(
        json_at(&settings)["hooks"]["Stop"][0]["hooks"][0]["command"],
        "/opt/other"
    );
    assert!(!home.join(".codex/hooks.json").exists());
}

#[test]
fn the_agents_own_variables_name_the_files_install_and_uninstall_touch() {
    let t = T::new();
    let home = agent_dirs(&t);
    let claude_dir = t.tmp.join("claude-config");
    let codex_dir = t.tmp.join("codex-home");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::create_dir_all(&codex_dir).unwrap();
    let home_claude = home.join(".claude/settings.json");
    let home_codex = home.join(".codex/hooks.json");
    let theirs = r#"{"model": "x"}"#;
    fs::write(&home_claude, theirs).unwrap();
    fs::write(&home_codex, "{}").unwrap();
    let env: [(&str, &Path); 2] = [
        ("CLAUDE_CONFIG_DIR", &claude_dir),
        ("CODEX_HOME", &codex_dir),
    ];
    let ok = |o: Output| assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    ok(t.run(&["install"], &env));
    assert!(!ours(&json_at(&claude_dir.join("settings.json"))).is_empty());
    assert!(!ours(&json_at(&codex_dir.join("hooks.json"))).is_empty());
    assert_eq!(fs::read_to_string(&home_claude).unwrap(), theirs);
    assert_eq!(fs::read_to_string(&home_codex).unwrap(), "{}");

    ok(t.run(&["uninstall"], &env));
    assert!(!claude_dir.join("settings.json").exists());
    assert!(!codex_dir.join("hooks.json").exists());
    assert_eq!(fs::read_to_string(&home_claude).unwrap(), theirs);
    assert_eq!(fs::read_to_string(&home_codex).unwrap(), "{}");

    t.mux(&["install"]);
    assert!(!ours(&json_at(&home_claude)).is_empty());
    assert!(!ours(&json_at(&home_codex)).is_empty());
    assert!(!claude_dir.join("settings.json").exists());
    assert!(!codex_dir.join("hooks.json").exists());
}

#[test]
fn an_empty_variable_or_a_missing_directory_wires_nothing_else() {
    let t = T::new();
    let home = agent_dirs(&t);
    let missing = t.tmp.join("no-such-dir");
    let empty = Path::new("");
    let env: [(&str, &Path); 2] = [("CLAUDE_CONFIG_DIR", empty), ("CODEX_HOME", &missing)];
    let o = t.run(&["install"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!ours(&json_at(&home.join(".claude/settings.json"))).is_empty());
    assert!(!home.join(".codex/hooks.json").exists());
    assert!(!missing.exists());
}

fn gemini_settings() -> Value {
    json!({"theme": "dark", "hooks": {"BeforeTool": [{"matcher": "^mcp_", "hooks": [
        {"type": "command", "command": "/opt/other-guard", "name": "other"}]}]}})
}

#[test]
fn install_wires_gemini_cli_and_uninstall_takes_only_its_entries() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let file = home.join(".gemini/settings.json");
    fs::write(&file, gemini_settings().to_string()).unwrap();
    let mut said = String::new();
    for _ in 0..2 {
        said = String::from_utf8_lossy(&t.mux(&["install"]).stdout).into_owned();
    }
    assert!(said.contains("Gemini CLI runs hooks only in folders it trusts"));
    let config = json_at(&file);
    assert_eq!(config["theme"], "dark");
    let found = ours(&config);
    assert_eq!(
        events(&found),
        [
            "AfterAgent",
            "AfterTool",
            "BeforeTool",
            "SessionEnd",
            "SessionStart"
        ]
    );
    let matcher = |event: &str| found.iter().find(|(e, ..)| e == event).unwrap().1.clone();
    assert_eq!(
        matcher("BeforeTool").as_deref(),
        Some("^(run_shell_command|write_file|replace)$")
    );
    assert_eq!(matcher("AfterTool").as_deref(), Some("^run_shell_command$"));
    for event in ["AfterAgent", "SessionStart", "SessionEnd"] {
        assert_eq!(matcher(event), None, "{event}");
    }
    for (event, _, h) in &found {
        assert_eq!(h["type"], "command", "{event}");
        assert_eq!(h["name"], "git-mux", "{event}");
        assert_eq!(h["timeout"], 30000, "{event}");
        assert!(h.get("statusMessage").is_none(), "{event}");
        let command = h["command"].as_str().unwrap();
        assert!(command.starts_with("[ -x '"), "{command}");
        assert!(command.ends_with("git-mux' hook gemini"), "{command}");
        assert!(command.contains("|| exit 0; exec '"), "{command}");
    }
    let before = config["hooks"]["BeforeTool"].as_array().unwrap();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0]["hooks"][0]["command"], "/opt/other-guard");

    t.mux(&["uninstall"]);
    assert_eq!(json_at(&file), gemini_settings());
}

#[test]
fn gemini_cli_home_replaces_the_home_directory() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let home_file = home.join(".gemini/settings.json");
    let theirs = gemini_settings().to_string();
    fs::write(&home_file, &theirs).unwrap();
    let gemini_home = t.tmp.join("gemini-home");
    fs::create_dir_all(gemini_home.join(".gemini")).unwrap();
    let env: [(&str, &Path); 1] = [("GEMINI_CLI_HOME", &gemini_home)];

    assert!(t.run(&["install"], &env).status.success());
    let wired = json_at(&gemini_home.join(".gemini/settings.json"));
    assert_eq!(ours(&wired).len(), 5);
    assert_eq!(fs::read_to_string(&home_file).unwrap(), theirs);

    assert!(t.run(&["uninstall"], &env).status.success());
    assert!(!gemini_home.join(".gemini/settings.json").exists());
    assert_eq!(fs::read_to_string(&home_file).unwrap(), theirs);

    let bare = t.tmp.join("bare-home");
    fs::create_dir_all(&bare).unwrap();
    let env: [(&str, &Path); 1] = [("GEMINI_CLI_HOME", &bare)];
    assert!(t.run(&["install"], &env).status.success());
    assert!(!bare.join(".gemini").exists());
    assert_eq!(fs::read_to_string(&home_file).unwrap(), theirs);

    let empty = Path::new("");
    let env: [(&str, &Path); 1] = [("GEMINI_CLI_HOME", empty)];
    assert!(t.run(&["install"], &env).status.success());
    assert_eq!(ours(&json_at(&home_file)).len(), 5);
}

#[test]
fn without_a_gemini_directory_nothing_is_written() {
    let t = T::new();
    let home = agent_dirs(&t);
    let said = String::from_utf8_lossy(&t.mux(&["install"]).stdout).into_owned();
    assert!(!home.join(".gemini").exists());
    assert!(!said.contains("Gemini"), "{said}");
}

#[test]
fn a_gemini_settings_file_that_is_not_json_is_refused_by_name() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let file = home.join(".gemini/settings.json");
    fs::write(&file, "{ // comment\n}").unwrap();
    let o = t.run(&["install"], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains(&*file.to_string_lossy()));
    assert_eq!(fs::read_to_string(&file).unwrap(), "{ // comment\n}");
}

#[test]
fn uninstall_leaves_a_settings_file_without_hooks_byte_identical() {
    let t = T::new();
    let home = t.tmp.join("home");
    let theirs = "{\"theme\":  \"dark\",\n \"tools\": {\"allowed\": []}}";
    let gemini = home.join(".gemini/settings.json");
    let local = t.repo.join(".claude/settings.local.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    t.mux(&["install", "--project"]);
    fs::create_dir_all(home.join(".gemini")).unwrap();
    for file in [&gemini, &local] {
        fs::write(file, theirs).unwrap();
    }
    t.mux(&["uninstall"]);
    for file in [&gemini, &local] {
        assert_eq!(fs::read_to_string(file).unwrap(), theirs, "{file:?}");
    }
}

#[test]
fn settings_of_other_shapes_survive_install_and_uninstall() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let file = home.join(".gemini/settings.json");
    let theirs = json!({"hooks": {
        "disabled": ["x"],
        "Notification": [],
        "Other": "not a list",
        "BeforeTool": [{"matcher": "^mcp_"}, {"matcher": "^other$", "hooks": []}]}});
    fs::write(&file, theirs.to_string()).unwrap();
    t.mux(&["install"]);
    let wired = json_at(&file);
    assert_eq!(wired["hooks"]["BeforeTool"].as_array().unwrap().len(), 3);
    assert_eq!(wired["hooks"]["disabled"], json!(["x"]));
    t.mux(&["uninstall"]);
    assert_eq!(json_at(&file), theirs);
}

#[test]
fn a_settings_file_whose_top_level_is_not_an_object_is_refused_by_name() {
    let t = T::new();
    let home = t.tmp.join("home");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let file = home.join(".gemini/settings.json");
    fs::write(&file, "[]").unwrap();
    let o = t.run(&["install"], &[]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains(&*file.to_string_lossy()), "{err}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "[]");
}

fn second_checkout(t: &T) -> PathBuf {
    let second = t.tmp.join("second");
    fs::create_dir_all(&second).unwrap();
    t.sh(&second, &["git", "init", "-q", "-b", "main"]);
    second
}

fn refused_install(t: &T, second: &Path, file: &Path, key: &str) {
    let o = cmd(BIN, &t.tmp)
        .arg("install")
        .current_dir(second)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains(&*file.to_string_lossy()), "{err}");
    assert!(err.contains(key), "{err}");
    let registry = fs::read_to_string(t.tmp.join("config/gitmux/repos")).unwrap();
    assert_eq!(registry, format!("{}\n", t.repo.display()), "not enrolled");
    let state = second.join(".git/gitmux");
    if state.exists() {
        assert_eq!(fs::read_dir(&state).unwrap().count(), 0, "nothing written");
    }
    assert!(!second.join(".git/hooks/reference-transaction").exists());
    assert!(!second.join(".git/hooks/pre-commit").exists());
}

#[test]
fn install_refuses_a_hooks_value_that_is_not_an_object_before_it_changes_anything() {
    let t = T::new();
    let home = t.tmp.join("home");
    let second = second_checkout(&t);
    fs::create_dir_all(home.join(".gemini")).unwrap();
    let gemini = home.join(".gemini/settings.json");
    fs::write(&gemini, r#"{"hooks": []}"#).unwrap();
    let claude = home.join(".claude/settings.json");
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    fs::write(&claude, r#"{"model": "x"}"#).unwrap();
    refused_install(&t, &second, &gemini, "hooks");
    assert_eq!(fs::read_to_string(&gemini).unwrap(), r#"{"hooks": []}"#);
    assert_eq!(fs::read_to_string(&claude).unwrap(), r#"{"model": "x"}"#);
}

#[test]
fn install_refuses_an_event_that_is_not_a_list_before_it_changes_anything() {
    let t = T::new();
    let home = t.tmp.join("home");
    let second = second_checkout(&t);
    let claude = home.join(".claude/settings.json");
    let theirs = r#"{"hooks": {"PreToolUse": "x"}}"#;
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    fs::write(&claude, theirs).unwrap();
    let codex = home.join(".codex/hooks.json");
    fs::create_dir_all(codex.parent().unwrap()).unwrap();
    fs::write(&codex, "{}").unwrap();
    refused_install(&t, &second, &claude, "PreToolUse");
    assert_eq!(fs::read_to_string(&claude).unwrap(), theirs);
    assert_eq!(fs::read_to_string(&codex).unwrap(), "{}");
}

#[test]
fn uninstall_validates_every_settings_file_before_it_removes_anything() {
    let t = T::new();
    let claude = t.tmp.join("home/.claude/settings.json");
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "t: a", "a.txt"]);
    let slot = t.tmp.join("slots/mux-verify-0");
    assert!(slot.exists());
    fs::create_dir_all(claude.parent().unwrap()).unwrap();
    fs::write(&claude, "{ // comment\n}").unwrap();
    let o = t.run(&["uninstall"], &[]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(err.contains(&*claude.to_string_lossy()), "{err}");
    let guard = fs::read_to_string(t.repo.join(".git/hooks/pre-commit")).unwrap();
    assert!(guard.contains("git-mux"), "the guard line stays");
    assert_eq!(
        fs::read_to_string(t.tmp.join("config/gitmux/repos")).unwrap(),
        format!("{}\n", t.repo.display())
    );
    assert!(slot.exists() && t.state().exists());
    assert_eq!(fs::read_to_string(&claude).unwrap(), "{ // comment\n}");
    fs::write(&claude, "{}").unwrap();
    t.mux(&["uninstall"]);
    assert!(!t.tmp.join("config/gitmux/repos").exists());
    assert!(!slot.exists() && !t.state().exists());
}
