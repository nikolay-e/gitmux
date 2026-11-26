use crate::guard;
use crate::repo::{Repo, write_atomic};
use crate::run::Result;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

fn registry() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .unwrap_or_default();
    base.join("gitmux").join("repos")
}

/// The checkouts whose agents the hook coordinates; every git-mux run in one enrolls it.
pub fn registered_roots() -> Vec<PathBuf> {
    fs::read_to_string(registry())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn ensure(repo: &Repo) -> Result<()> {
    guard::ensure(repo)?;
    let _ = fs::remove_dir_all(repo.state.join("bin"));
    let root = repo.main_root();
    let mut roots = registered_roots();
    if !roots.contains(&root) {
        roots.push(root);
        let file = registry();
        fs::create_dir_all(file.parent().unwrap())?;
        let text: String = roots.iter().map(|r| format!("{}\n", r.display())).collect();
        write_atomic(&file, text.as_bytes(), None)?;
    }
    Ok(())
}

fn is_ours(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| c.contains("git-mux") || c.contains("/mux/bin/hook"))
}

/// Replaces any earlier git-mux entry in one agent's hook config with the current binary.
fn wire(file: &Path, command: &str, pre: &str, post: &str) -> Result<bool> {
    let mut config: Value = match fs::read(file) {
        Ok(data) => serde_json::from_slice(&data)
            .map_err(|e| crate::run::Refused(format!("{}: {e}", file.display())))?,
        Err(_) => json!({}),
    };
    let before = config.clone();
    if !config["hooks"].is_object() {
        config["hooks"] = json!({});
    }
    for (event, matcher) in [("PreToolUse", pre), ("PostToolUse", post)] {
        let mut groups: Vec<Value> = config["hooks"][event]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for g in &mut groups {
            if let Some(hs) = g["hooks"].as_array_mut() {
                hs.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
        let hook = json!({"type": "command", "command": command, "timeout": 10, "statusMessage": "git-mux"});
        groups.push(json!({"matcher": matcher, "hooks": [hook]}));
        config["hooks"][event] = Value::Array(groups);
    }
    if config == before {
        return Ok(false);
    }
    fs::create_dir_all(file.parent().unwrap())?;
    write_atomic(
        file,
        format!("{}\n", serde_json::to_string_pretty(&config).unwrap()).as_bytes(),
        None,
    )?;
    Ok(true)
}

/// `git mux install`: the git guards, plus the tool hooks of every agent installed for this user.
pub fn install(repo: &Repo) -> Result<()> {
    ensure(repo)?;
    let exe = std::env::current_exe()?;
    let command = format!(
        "'{}' hook",
        exe.display().to_string().replace('\'', r"'\''")
    );
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let agents: [(&str, PathBuf, &str, &str); 2] = [
        (
            "Claude Code",
            home.join(".claude/settings.json"),
            "Bash|Edit|Write|MultiEdit|NotebookEdit",
            "Bash",
        ),
        (
            "Codex",
            home.join(".codex/hooks.json"),
            "^(Bash|apply_patch|Edit|Write)$",
            "^Bash$",
        ),
    ];
    for (name, file, pre, post) in agents {
        if !file.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let changed = wire(&file, &command, pre, post)?;
        println!(
            "git-mux: {name} hooks in {} {}",
            file.display(),
            if changed {
                "updated"
            } else {
                "already current"
            }
        );
        if changed && name == "Codex" {
            println!("git-mux: Codex runs a changed hook only after you trust it once with /hooks");
        }
    }
    println!(
        "git-mux: {} is enrolled; agents commit with `git mux commit -m MSG -- PATH...`",
        repo.main_root().display()
    );
    Ok(())
}
