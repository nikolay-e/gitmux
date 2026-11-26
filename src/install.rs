use crate::guard;
use crate::repo::{Repo, write_atomic};
use crate::run::{Cmd, Result, refuse};
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

/// One settings file, refused by name when git-mux could not edit it without losing something: it
/// does not parse, its top level is not an object, `hooks` is present and not an object, or one of
/// `events` holds a value that is not an array. A missing file is an empty one.
fn read_config(file: &Path, events: &[&str]) -> Result<Value> {
    let Ok(data) = fs::read(file) else {
        return Ok(json!({}));
    };
    let config: Value = serde_json::from_slice(&data)
        .map_err(|e| crate::run::Refused(format!("{}: {e}", file.display())))?;
    if !config.is_object() {
        refuse!("{}: the top level is not a JSON object", file.display());
    }
    let Some(hooks) = config.get("hooks") else {
        return Ok(config);
    };
    let Some(wired) = hooks.as_object() else {
        refuse!("{}: hooks is not a JSON object", file.display());
    };
    for event in events {
        if wired.get(*event).is_some_and(|groups| !groups.is_array()) {
            refuse!("{}: hooks.{event} is not a JSON array", file.display());
        }
    }
    Ok(config)
}

fn write_config(file: &Path, config: &Value) -> Result<()> {
    fs::create_dir_all(file.parent().unwrap())?;
    write_atomic(
        file,
        format!("{}\n", serde_json::to_string_pretty(config).unwrap()).as_bytes(),
        None,
    )
}

/// Removes every git-mux handler, whatever event it sits on, and the groups and events that
/// leave empty. Whatever else the file holds, a value of another shape included, stays as it was.
fn strip_ours(config: &mut Value) {
    let Some(events) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    events.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        let before = groups.len();
        groups.retain_mut(|group| {
            let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let had = hooks.len();
            hooks.retain(|h| !is_ours(h));
            !hooks.is_empty() || hooks.len() == had
        });
        groups.len() == before || !groups.is_empty()
    });
}

/// Replaces any earlier git-mux entry in one hook config with `hook` on each of `events`.
fn wire(file: &Path, events: &[(&str, Option<&str>)], hook: &Value) -> Result<bool> {
    let mut config = read_config(file, &names(events))?;
    let before = config.clone();
    strip_ours(&mut config);
    let wired = config
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    for (event, matcher) in events {
        let groups = wired.entry(*event).or_insert_with(|| json!([]));
        groups.as_array_mut().unwrap().push(match matcher {
            Some(m) => json!({"matcher": m, "hooks": [hook]}),
            None => json!({"hooks": [hook]}),
        });
    }
    if config == before {
        return Ok(false);
    }
    write_config(file, &config)?;
    Ok(true)
}

/// Takes git-mux out of one hook config, leaving every other handler and key.
fn unwire(file: &Path, events: &[(&str, Option<&str>)]) -> Result<bool> {
    if !file.is_file() {
        return Ok(false);
    }
    let mut config = read_config(file, &names(events))?;
    let before = config.clone();
    strip_ours(&mut config);
    if config == before {
        return Ok(false);
    }
    let root = config.as_object_mut().unwrap();
    if root
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|events| events.is_empty())
    {
        root.remove("hooks");
    }
    if root.is_empty() {
        fs::remove_file(file)?;
    } else {
        write_config(file, &config)?;
    }
    Ok(true)
}

fn names<'a>(events: &[(&'a str, Option<&str>)]) -> Vec<&'a str> {
    events.iter().map(|(event, _)| *event).collect()
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

struct Agent {
    name: &'static str,
    file: PathBuf,
    /// Gemini CLI: its own dialect (`hook gemini`), and a timeout in milliseconds.
    gemini: bool,
    events: Vec<(&'static str, Option<&'static str>)>,
}

/// An agent's user-level config file: the directory its own variable names when that is set and
/// non-empty, else `$HOME/<default_dir>`.
fn config_file(var: &str, default_dir: &str, name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(default_dir)))?;
    Some(dir.join(name))
}

/// Claude Code, Codex and Gemini CLI, for those whose config file resolves and whose directory
/// exists.
fn agents() -> Vec<Agent> {
    let claude = config_file("CLAUDE_CONFIG_DIR", ".claude", "settings.json").map(|file| Agent {
        name: "Claude Code",
        file,
        gemini: false,
        events: vec![
            ("PreToolUse", Some("Bash|Edit|Write|MultiEdit|NotebookEdit")),
            ("PostToolUse", Some("Bash")),
            ("PostToolUseFailure", Some("Bash")),
            ("PermissionDenied", Some("Bash")),
            ("Stop", None),
            ("SubagentStop", None),
            ("SessionStart", None),
            ("SessionEnd", None),
        ],
    });
    let codex = config_file("CODEX_HOME", ".codex", "hooks.json").map(|file| Agent {
        name: "Codex",
        file,
        gemini: false,
        events: vec![
            ("PreToolUse", Some("^(Bash|apply_patch|Edit|Write)$")),
            ("PostToolUse", Some("^Bash$")),
            ("Stop", None),
            ("Interrupt", None),
            ("SessionStart", None),
            ("SessionEnd", None),
        ],
    });
    // GEMINI_CLI_HOME replaces the home directory, so `.gemini` stays under it.
    let gemini = std::env::var_os("GEMINI_CLI_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .map(|home| Agent {
            name: "Gemini CLI",
            file: home.join(".gemini/settings.json"),
            gemini: true,
            events: vec![
                (
                    "BeforeTool",
                    Some("^(run_shell_command|write_file|replace)$"),
                ),
                ("AfterTool", Some("^run_shell_command$")),
                ("AfterAgent", None),
                ("SessionStart", None),
                ("SessionEnd", None),
            ],
        });
    [claude, codex, gemini]
        .into_iter()
        .flatten()
        .filter(|a| a.file.parent().is_some_and(Path::is_dir))
        .collect()
}

fn project_file(repo: &Repo) -> PathBuf {
    repo.main_root().join(".claude/settings.local.json")
}

const PROJECT_EVENTS: [(&str, Option<&str>); 4] = [
    ("PreToolUse", Some("Bash|Edit|Write|MultiEdit|NotebookEdit")),
    ("PostToolUse", Some("Bash")),
    ("PostToolUseFailure", Some("Bash")),
    ("PermissionDenied", Some("Bash")),
];

/// The checkout's own Claude Code settings, which no clone shares: it holds the binary's absolute
/// path. Exec form with `onFailure: "block"`, so a hook that cannot run refuses the call here
/// while every other project stays open. Git is told to ignore the file.
fn wire_project(repo: &Repo, exe: &Path) -> Result<()> {
    let file = project_file(repo);
    let hook = json!({"type": "command", "command": exe, "args": ["hook"], "onFailure": "block", "timeout": 30});
    let changed = wire(&file, &PROJECT_EVENTS, &hook)?;
    let checked = Cmd::git(&repo.main_root())
        .env_remove("GIT_LITERAL_PATHSPECS")
        .args(["check-ignore", "-q", "--", ".claude/settings.local.json"])
        .out()?;
    if checked.code > 1 {
        refuse!("git check-ignore failed: {}", checked.stderr.trim());
    }
    let ignored = checked.ok();
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
    for agent in agents() {
        // A missing binary lets the call through everywhere but in a checkout with the project hook.
        let command = |args: &str| format!("[ -x {0} ] || exit 0; exec {0} {args}", quoted(&exe));
        let hook = if agent.gemini {
            json!({"type": "command", "command": command("hook gemini"), "name": "git-mux", "timeout": 30000})
        } else {
            json!({"type": "command", "command": command("hook"), "timeout": 30, "statusMessage": "git-mux"})
        };
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
            println!("git-mux: Codex runs a changed hook only after you trust it once with /hooks");
        }
        if agent.gemini {
            println!(
                "git-mux: Gemini CLI runs hooks only in folders it trusts: trust this checkout in Gemini, or git-mux's guard does not run there"
            );
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

/// The settings files `install` would write, read before anything else changes: an invalid one
/// refuses the install with the checkout not enrolled.
pub fn validate_install(repo: &Repo, project: bool) -> Result<()> {
    for agent in agents() {
        read_config(&agent.file, &names(&agent.events))?;
    }
    if project {
        read_config(&project_file(repo), &names(&PROJECT_EVENTS))?;
    }
    Ok(())
}

fn remaining_roots(repo: &Repo) -> Vec<PathBuf> {
    let root = repo.main_root();
    registered_roots()
        .into_iter()
        .filter(|r| *r != root && r.is_dir())
        .collect()
}

/// The settings files `uninstall` would edit, read before the guards, slots and registry go.
pub fn validate_uninstall(repo: &Repo) -> Result<()> {
    read_config(&project_file(repo), &names(&PROJECT_EVENTS))?;
    if remaining_roots(repo).is_empty() {
        for agent in agents() {
            read_config(&agent.file, &names(&agent.events))?;
        }
    }
    Ok(())
}

/// `git mux uninstall`: the checkout leaves git-mux (guards, slots, state, project hooks,
/// registry entry); the agents' hooks go with the last checkout.
pub fn uninstall(repo: &Repo) -> Result<()> {
    guard::remove(repo)?;
    slot::remove_all(repo)?;
    if unwire(&project_file(repo), &PROJECT_EVENTS)? {
        println!("git-mux: project hooks removed");
    }
    fs::remove_dir_all(&repo.state)?;
    let root = repo.main_root();
    let remaining = remaining_roots(repo);
    save_registry(&remaining)?;
    if remaining.is_empty() {
        for agent in agents() {
            if unwire(&agent.file, &agent.events)? {
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
