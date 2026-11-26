use crate::guard;
use crate::repo::{Repo, write_atomic};
use crate::run::{Cmd, Result};
use crate::slot;
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

/// The checkouts whose agents the hook coordinates; only `git mux install` enrolls one.
pub fn registered_roots() -> Vec<PathBuf> {
    fs::read_to_string(registry())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn save_registry(roots: &[PathBuf]) -> Result<()> {
    let file = registry();
    if roots.is_empty() {
        let _ = fs::remove_file(&file);
        return Ok(());
    }
    fs::create_dir_all(file.parent().unwrap())?;
    let text: String = roots.iter().map(|r| format!("{}\n", r.display())).collect();
    write_atomic(&file, text.as_bytes(), None)
}

/// Drops the entries whose directory is gone, enrolls the checkout when `enroll` is set, and
/// repairs the guards of an enrolled checkout; any other checkout is left untouched. Returns the
/// notes about what the guards cannot cover.
pub fn ensure(repo: &Repo, enroll: bool) -> Result<Vec<String>> {
    let root = repo.main_root();
    let listed = registered_roots();
    let mut roots: Vec<PathBuf> = listed.iter().filter(|r| r.is_dir()).cloned().collect();
    if enroll && !roots.contains(&root) {
        roots.push(root.clone());
    }
    if roots != listed {
        save_registry(&roots)?;
    }
    if roots.contains(&root) {
        guard::ensure(repo)
    } else {
        Ok(Vec::new())
    }
}

fn is_ours(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| c.contains("git-mux"))
}

fn read_config(file: &Path) -> Result<Value> {
    match fs::read(file) {
        Ok(data) => serde_json::from_slice(&data)
            .map_err(|e| crate::run::Refused(format!("{}: {e}", file.display()))),
        Err(_) => Ok(json!({})),
    }
}

fn write_config(file: &Path, config: &Value) -> Result<()> {
    fs::create_dir_all(file.parent().unwrap())?;
    write_atomic(
        file,
        format!("{}\n", serde_json::to_string_pretty(config).unwrap()).as_bytes(),
        None,
    )
}

/// Removes every git-mux handler, whatever event it sits on, and the groups that leave empty.
fn strip_ours(config: &mut Value) {
    let Some(events) = config["hooks"].as_object_mut() else {
        return;
    };
    for groups in events.values_mut().filter_map(Value::as_array_mut) {
        for g in groups.iter_mut() {
            if let Some(hs) = g["hooks"].as_array_mut() {
                hs.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
    }
}

/// Replaces any earlier git-mux entry in one hook config with `hook` on each of `events`.
fn wire(file: &Path, events: &[(&str, Option<&str>)], hook: &Value) -> Result<bool> {
    let mut config = read_config(file)?;
    let before = config.clone();
    strip_ours(&mut config);
    if !config["hooks"].is_object() {
        config["hooks"] = json!({});
    }
    for (event, matcher) in events {
        let mut groups = config["hooks"][event]
            .as_array()
            .cloned()
            .unwrap_or_default();
        groups.push(match matcher {
            Some(m) => json!({"matcher": m, "hooks": [hook]}),
            None => json!({"hooks": [hook]}),
        });
        config["hooks"][event] = Value::Array(groups);
    }
    if config == before {
        return Ok(false);
    }
    write_config(file, &config)?;
    Ok(true)
}

/// Takes git-mux out of one hook config, leaving every other handler and key.
fn unwire(file: &Path) -> Result<bool> {
    if !file.is_file() {
        return Ok(false);
    }
    let mut config = read_config(file)?;
    let before = config.clone();
    strip_ours(&mut config);
    if let Some(events) = config["hooks"].as_object_mut() {
        events.retain(|_, v| v.as_array().is_none_or(|g| !g.is_empty()));
        if events.is_empty() {
            config.as_object_mut().unwrap().remove("hooks");
        }
    }
    if config == before {
        return Ok(false);
    }
    if config.as_object().is_some_and(|o| o.is_empty()) {
        fs::remove_file(file)?;
    } else {
        write_config(file, &config)?;
    }
    Ok(true)
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

struct Agent {
    name: &'static str,
    file: PathBuf,
    events: Vec<(&'static str, Option<&'static str>)>,
}

/// Claude Code and Codex, for those whose config directory exists.
fn agents(home: &Path) -> Vec<Agent> {
    let claude_edit = "Bash|Edit|Write|MultiEdit|NotebookEdit";
    vec![
        Agent {
            name: "Claude Code",
            file: home.join(".claude/settings.json"),
            events: vec![
                ("PreToolUse", Some(claude_edit)),
                ("PostToolUse", Some("Bash")),
                ("PostToolUseFailure", Some("Bash")),
                ("PermissionDenied", Some("Bash")),
                ("Stop", None),
                ("SubagentStop", None),
                ("SessionStart", None),
                ("SessionEnd", None),
            ],
        },
        Agent {
            name: "Codex",
            file: home.join(".codex/hooks.json"),
            events: vec![
                ("PreToolUse", Some("^(Bash|apply_patch|Edit|Write)$")),
                ("PostToolUse", Some("^Bash$")),
                ("Stop", None),
                ("Interrupt", None),
                ("SessionStart", None),
                ("SessionEnd", None),
            ],
        },
    ]
    .into_iter()
    .filter(|a| a.file.parent().is_some_and(Path::is_dir))
    .collect()
}

fn project_file(repo: &Repo) -> PathBuf {
    repo.main_root().join(".claude/settings.local.json")
}

/// The checkout's own Claude Code settings, which no clone shares: it holds the binary's absolute
/// path. Exec form with `onFailure: "block"`, so a hook that cannot run refuses the call here
/// while every other project stays open. Git is told to ignore the file.
fn wire_project(repo: &Repo, exe: &Path) -> Result<()> {
    let file = project_file(repo);
    let hook = json!({"type": "command", "command": exe, "args": ["hook"], "onFailure": "block", "timeout": 30});
    let events = [
        ("PreToolUse", Some("Bash|Edit|Write|MultiEdit|NotebookEdit")),
        ("PostToolUse", Some("Bash")),
        ("PostToolUseFailure", Some("Bash")),
        ("PermissionDenied", Some("Bash")),
    ];
    let changed = wire(&file, &events, &hook)?;
    let ignored = Cmd::git(&repo.main_root())
        .args(["check-ignore", "-q", "--"])
        .arg(&file)
        .out()?
        .ok();
    if !ignored {
        let exclude = repo.common.join("info/exclude");
        fs::create_dir_all(exclude.parent().unwrap())?;
        let mut text = fs::read_to_string(&exclude).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("/.claude/settings.local.json\n");
        write_atomic(&exclude, text.as_bytes(), None)?;
    }
    println!(
        "git-mux: project hooks in {} {}",
        file.display(),
        if changed {
            "updated"
        } else {
            "already current"
        }
    );
    Ok(())
}

/// `git mux install`: the tool hooks of every agent installed for this user (and, with
/// `project`, the checkout's own fail-closed Claude Code hooks). Enrolling and the git guards
/// are `ensure`'s.
pub fn install(repo: &Repo, project: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    // A missing binary lets the call through everywhere but in a checkout with the project hook.
    let command = format!("[ -x {0} ] || exit 0; exec {0} hook", quoted(&exe));
    let hook =
        json!({"type": "command", "command": command, "timeout": 30, "statusMessage": "git-mux"});
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for agent in agents(&home) {
            let changed = wire(&agent.file, &agent.events, &hook)?;
            println!(
                "git-mux: {} hooks in {} {}",
                agent.name,
                agent.file.display(),
                if changed {
                    "updated"
                } else {
                    "already current"
                }
            );
            if changed && agent.name == "Codex" {
                println!(
                    "git-mux: Codex runs a changed hook only after you trust it once with /hooks"
                );
            }
        }
    }
    if project {
        wire_project(repo, &exe)?;
    }
    println!(
        "git-mux: {} is enrolled; agents commit with `git mux commit -m MSG -- PATH...`",
        repo.main_root().display()
    );
    Ok(())
}

/// `git mux uninstall`: the checkout leaves git-mux (guards, slots, state, project hooks,
/// registry entry); the agents' hooks go with the last checkout.
pub fn uninstall(repo: &Repo) -> Result<()> {
    guard::remove(repo)?;
    slot::remove_all(repo)?;
    if unwire(&project_file(repo))? {
        println!("git-mux: project hooks removed");
    }
    fs::remove_dir_all(&repo.state)?;
    let root = repo.main_root();
    let remaining: Vec<PathBuf> = registered_roots()
        .into_iter()
        .filter(|r| *r != root && r.is_dir())
        .collect();
    save_registry(&remaining)?;
    if remaining.is_empty()
        && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
    {
        for agent in agents(&home) {
            if unwire(&agent.file)? {
                println!(
                    "git-mux: {} hooks removed from {}",
                    agent.name,
                    agent.file.display()
                );
            }
        }
    }
    println!("git-mux: {} is no longer enrolled", root.display());
    Ok(())
}
