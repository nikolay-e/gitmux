mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;

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
