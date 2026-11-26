//! The Claude Code / Codex Pre- and PostToolUse hook. Exit 2 with the reason on stderr is the one
//! blocking signal both honour, so a refusal and a crash both end that way.
use crate::commit::expand;
use crate::install::registered_roots;
use crate::lease::{agent_process, alive, holder, short};
use crate::repo::{Repo, inside, now, read_json};
use crate::run::{Result, zsplit};
use crate::shell::{Kind, commit_paths, guard_git, invocations, patch_paths};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit", "apply_patch"];
const SNAPSHOTS: usize = 8;

enum Decision {
    Allow,
    Deny(String),
    Note(String),
}

pub fn main() -> ExitCode {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let roots = registered_roots();
    // Most calls concern no enrolled checkout: answer them without parsing or running git.
    if !roots.iter().any(|r| {
        r.file_name()
            .is_some_and(|n| raw.contains(&*n.to_string_lossy()))
    }) {
        return ExitCode::SUCCESS;
    }
    let decision = serde_json::from_str::<Value>(&raw)
        .map_err(|e| crate::run::Refused(format!("unreadable hook payload: {e}")))
        .and_then(|p| decide(&p, &roots));
    match decision {
        Ok(Decision::Allow) => ExitCode::SUCCESS,
        Ok(Decision::Note(text)) => {
            println!(
                "{}",
                json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text}})
            );
            ExitCode::SUCCESS
        }
        Ok(Decision::Deny(reason)) => {
            eprintln!("git-mux: {reason}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("git-mux: the shared-checkout guard failed, so the call is refused: {e}");
            ExitCode::from(2)
        }
    }
}

fn mentions(command: &str, root: &Path) -> bool {
    let mut forms = vec![root.to_string_lossy().into_owned()];
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rel) = root.strip_prefix(&home)
    {
        forms.push(format!("~/{}", rel.display()));
        forms.push(format!("$HOME/{}", rel.display()));
    }
    forms.iter().any(|f| command.contains(f.as_str()))
}

fn root_of<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots.iter().find(|r| inside(path, r))
}

fn decide(p: &Value, roots: &[PathBuf]) -> Result<Decision> {
    let tool = p["tool_name"].as_str().unwrap_or("");
    let data = &p["tool_input"];
    let cwd = p["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let post = p["hook_event_name"].as_str() == Some("PostToolUse");
    let session = p["session_id"].as_str().unwrap_or("unknown");

    if tool == "Bash" {
        let command = match &data["command"] {
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            v => v.as_str().unwrap_or("").to_string(),
        };
        let mut touched: BTreeSet<&PathBuf> = roots
            .iter()
            .filter(|r| inside(&cwd, r) || mentions(&command, r))
            .collect();
        if post {
            let mut notes = Vec::new();
            for root in touched {
                notes.extend(shell_snapshot(&Repo::open(root)?, p, session)?);
            }
            return Ok(if notes.is_empty() {
                Decision::Allow
            } else {
                Decision::Note(notes.join("\n"))
            });
        }
        let mut claims = Vec::new();
        for inv in invocations(&command, &cwd) {
            let Some(root) = root_of(&inv.dir, roots) else {
                continue;
            };
            touched.insert(root);
            if inv.kind == Kind::Git {
                if let Some(why) = guard_git(&inv.args) {
                    let shown: String = inv.args.join(" ").chars().take(60).collect();
                    return Ok(Decision::Deny(format!(
                        "`git {shown}` in the shared checkout {}: {why}.",
                        root.display()
                    )));
                }
            } else if inv.args.first().map(String::as_str) == Some("commit")
                && !inv.args.iter().any(|a| a == "--take")
            {
                claims.push((root, inv.dir, commit_paths(&inv.args[1..])));
            }
        }
        for (root, dir, paths) in claims {
            let repo = Repo::open(root)?;
            let Ok(paths) = expand(&repo, &paths, Some(&dir)) else {
                continue;
            };
            let held = repo.foreign_leases(session, &paths)?;
            if !held.is_empty() {
                return Ok(Decision::Deny(format!(
                    "{held} has uncommitted edits by another live agent; commit only your own paths, or pass --take when that session handed them over."
                )));
            }
        }
        for root in touched {
            shell_snapshot(&Repo::open(root)?, p, session)?;
        }
        return Ok(Decision::Allow);
    }

    if post || !EDIT_TOOLS.contains(&tool) {
        return Ok(Decision::Allow);
    }
    let files: Vec<PathBuf> = if tool == "apply_patch" {
        patch_paths(
            data["command"]
                .as_str()
                .or(data["input"].as_str())
                .unwrap_or(""),
        )
        .into_iter()
        .map(|f| cwd.join(f))
        .collect()
    } else {
        data["file_path"]
            .as_str()
            .or(data["notebook_path"].as_str())
            .map(PathBuf::from)
            .into_iter()
            .collect()
    };
    for root in roots {
        let mine: Vec<&PathBuf> = files.iter().filter(|f| inside(f, root)).collect();
        if mine.is_empty() {
            continue;
        }
        let repo = Repo::open(root)?;
        let paths: Vec<String> = mine
            .iter()
            .filter_map(|f| repo.rel(&f.to_string_lossy(), Some(&cwd)))
            .collect();
        let foreign = repo.claim(session, &paths)?;
        if !foreign.is_empty() {
            let owners: Vec<String> = foreign
                .iter()
                .map(|(p, s)| format!("{p} (session {})", short(s)))
                .collect();
            return Ok(Decision::Deny(format!(
                "{} has uncommitted edits by another live agent. Coordinate with it, wait for its commit, or work on another file.",
                owners.join(", ")
            )));
        }
    }
    Ok(Decision::Allow)
}

fn tree_state(repo: &Repo) -> Result<Map<String, Value>> {
    let out = repo
        .git()
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .raw()?;
    let mut items = zsplit(&out).into_iter();
    let mut state = Map::new();
    while let Some(item) = items.next() {
        let Some(path) = item.get(3..) else { continue };
        if item.starts_with(['R', 'C']) {
            items.next();
        }
        let v = fs::symlink_metadata(repo.root.join(path))
            .map(|m| json!([m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.size()]))
            .unwrap_or(Value::Null);
        state.insert(path.to_string(), v);
    }
    Ok(state)
}

fn call_log(path: &Path) -> Value {
    match read_json(path) {
        Some(v) if v["calls"].is_object() => v,
        _ => json!({"calls": {}}),
    }
}

fn file_name(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Sessions with a shell command running, or one that finished after `t0`; a log whose agent
/// process is gone is dropped.
fn active_sessions(calls: &Path, session: &str, t0: f64) -> BTreeSet<String> {
    let mut busy = BTreeSet::new();
    for entry in fs::read_dir(calls).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if path.extension().is_none_or(|e| e != "json") || stem == file_name(session) {
            continue;
        }
        let log = call_log(&path);
        let agent = log.get("agent").cloned().unwrap_or(Value::Null);
        if !alive(&agent) {
            let _ = fs::remove_file(&path);
        } else if log["calls"].as_object().is_some_and(|c| !c.is_empty())
            || log["last"].as_f64().unwrap_or(0.0) >= t0
        {
            busy.insert(stem);
        }
    }
    busy
}

/// A shell command can write anywhere, so the tree is compared around it. A changed path becomes
/// the session's only when no other session was active in the same window (an edit, or a shell
/// command running or finished inside it); a held path that changed is reported only when its
/// holder was idle, because then this command is the one that wrote it.
fn shell_snapshot(repo: &Repo, p: &Value, session: &str) -> Result<Option<String>> {
    let calls = repo.state.join("shell");
    let mine = calls.join(format!("{}.json", file_name(session)));
    let key = p["tool_use_id"].as_str().unwrap_or("").to_string();
    let post = p["hook_event_name"].as_str() == Some("PostToolUse");
    let (before, mut busy) = {
        let _lock = repo.lock("shell")?;
        fs::create_dir_all(&calls)?;
        let mut log = call_log(&mine);
        let before = if post {
            log["calls"].as_object_mut().unwrap().remove(&key)
        } else {
            if log.get("agent").is_none() {
                log["agent"] = Value::Object(agent_process(&repo.cfg.agents));
            }
            let map = log["calls"].as_object_mut().unwrap();
            map.insert(key, json!({"t0": now(), "state": tree_state(repo)?}));
            while map.len() > SNAPSHOTS {
                let oldest = map
                    .iter()
                    .min_by(|a, b| a.1["t0"].as_f64().partial_cmp(&b.1["t0"].as_f64()).unwrap())
                    .map(|(k, _)| k.clone())
                    .unwrap();
                map.remove(&oldest);
            }
            None
        };
        log["last"] = json!(now());
        fs::write(&mine, log.to_string())?;
        let busy = before
            .as_ref()
            .map(|b| active_sessions(&calls, session, b["t0"].as_f64().unwrap_or(0.0)))
            .unwrap_or_default();
        (before, busy)
    };
    let Some(before) = before else {
        return Ok(None);
    };
    let t0 = before["t0"].as_f64().unwrap_or(0.0);
    let now_state = tree_state(repo)?;
    let changed: Vec<String> = now_state
        .iter()
        .filter(|(path, v)| before["state"].get(path.as_str()).unwrap_or(&Value::Null) != *v)
        .map(|(path, _)| path.clone())
        .collect();
    if changed.is_empty() {
        return Ok(None);
    }
    let held = repo.leases()?;
    busy.extend(
        held.values()
            .filter(|v| v["touched"].as_f64().unwrap_or(0.0) >= t0)
            .filter_map(|v| v["session"].as_str().map(file_name)),
    );
    busy.remove(&file_name(session));
    let noted: Vec<&String> = changed
        .iter()
        .filter(|c| holder(&held, c).is_some_and(|s| s != session && alive(&held[c.as_str()])))
        .collect();
    let loose: Vec<String> = changed
        .iter()
        .filter(|c| !noted.contains(c))
        .cloned()
        .collect();
    if !loose.is_empty() && busy.is_empty() {
        repo.claim(session, &loose)?;
    }
    let noted: Vec<String> = noted
        .into_iter()
        .filter_map(|c| {
            holder(&held, c)
                .filter(|s| !busy.contains(&file_name(s)))
                .map(|s| format!("{c} (session {})", short(&s)))
        })
        .collect();
    if noted.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "git-mux: while this command ran, files another live agent is editing changed: {}. No other session was active meanwhile, so this command probably wrote them: restore their previous content and coordinate with that session.",
        noted.join(", ")
    )))
}
