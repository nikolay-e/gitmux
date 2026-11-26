mod common;

use common::*;
use serde_json::json;

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
