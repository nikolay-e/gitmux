//! The Claude Code / Codex tool and session hook. Exit 2 with the reason on stderr is the one
//! blocking signal both honour, so a refusal and a crash both end that way.
use crate::commit::expand;
use crate::install::registered_roots;
use crate::lease::{self, Owner, agent_process, alive, holder, short};
use crate::repo::{Repo, inside, normalize, now, posix, read_json, resolve};
use crate::run::{Result, zsplit};
use crate::shell::{Kind, argv_invocations, commit_paths, guard_git, invocations, patch_paths};
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
    /// Context for the model after a tool call.
    Note(String),
    /// Plain stdout, which the agents take as context at session start.
    Say(String),
}

/// An enrolled checkout that still exists: as registered, and with symlinks resolved.
struct Root {
    path: PathBuf,
    real: PathBuf,
}

impl Root {
    fn mentioned_in(&self, text: &str) -> bool {
        let mut forms = vec![
            self.path.to_string_lossy().into_owned(),
            self.real.to_string_lossy().into_owned(),
        ];
        if let Some(home) = std::env::var_os("HOME")
            && let Ok(rel) = self.path.strip_prefix(&home)
        {
            forms.push(format!("~/{}", rel.display()));
            forms.push(format!("$HOME/{}", rel.display()));
        }
        forms.iter().any(|f| text.contains(f.as_str()))
    }

    fn state(&self) -> PathBuf {
        self.real.join(".git/gitmux")
    }

    fn repo(&self) -> Result<Repo> {
        Repo::open(&self.real)
    }
}

fn live_roots() -> Vec<Root> {
    registered_roots()
        .into_iter()
        .filter_map(|path| {
            let real = fs::canonicalize(&path).ok()?;
            Some(Root { path, real })
        })
        .collect()
}

/// The shared checkout a path belongs to, and the path inside it: the nearest ancestor holding a
/// `.git` counts only when that is a directory and a registered root. A linked worktree or
/// submodule (`.git` file) and a nested repository are free, wherever they stand.
fn locate<'a>(path: &Path, roots: &'a [Root]) -> Option<(&'a Root, String)> {
    if !roots.iter().any(|r| inside(path, &r.real)) {
        return None;
    }
    let real = resolve(&normalize(path));
    let top = real
        .ancestors()
        .find(|a| fs::symlink_metadata(a.join(".git")).is_ok())?;
    if !fs::symlink_metadata(top.join(".git")).ok()?.is_dir() {
        return None;
    }
    let root = roots.iter().find(|r| r.real == top)?;
    Some((root, posix(real.strip_prefix(top).ok()?)))
}

fn shared_root_of<'a>(path: &Path, roots: &'a [Root]) -> Option<&'a Root> {
    locate(path, roots).map(|(root, _)| root)
}

fn names_git(command: &str) -> bool {
    command
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .any(|w| w == "git" || w == "git-mux")
}

pub fn main() -> ExitCode {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let roots = live_roots();
    if roots.is_empty() {
        return ExitCode::SUCCESS;
    }
    let payload = match serde_json::from_str::<Value>(&raw) {
        Ok(p) => p,
        Err(e) if roots.iter().any(|r| r.mentioned_in(&raw)) => {
            return refuse(&format!("unreadable hook payload: {e}"));
        }
        Err(_) => return ExitCode::SUCCESS,
    };
    let event = payload["hook_event_name"].as_str().unwrap_or("");
    match handle(&payload, event, &roots) {
        Ok(Decision::Allow) => ExitCode::SUCCESS,
        Ok(Decision::Say(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(Decision::Note(text)) => {
            println!(
                "{}",
                json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
            );
            ExitCode::SUCCESS
        }
        Ok(Decision::Deny(reason)) => {
            eprintln!("git-mux: {reason}");
            ExitCode::from(2)
        }
        // Exit 2 on Stop keeps the agent working, and the other lifecycle events cannot refuse
        // anything: housekeeping that fails is reported and the agent goes on.
        Err(e) if is_lifecycle(event) => {
            eprintln!("git-mux: {event} housekeeping failed: {e}");
            ExitCode::SUCCESS
        }
        Err(e) => refuse(&e.to_string()),
    }
}

fn refuse(why: &str) -> ExitCode {
    eprintln!("git-mux: the shared-checkout guard failed, so the call is refused: {why}");
    ExitCode::from(2)
}

fn is_lifecycle(event: &str) -> bool {
    matches!(
        event,
        "PermissionDenied" | "Stop" | "Interrupt" | "SubagentStop" | "SessionStart" | "SessionEnd"
    )
}

fn handle(p: &Value, event: &str, roots: &[Root]) -> Result<Decision> {
    let cwd = p["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let session = p["session_id"].as_str().unwrap_or("unknown");
    let agent = p["agent_id"].as_str();
    match event {
        "SessionStart" => Ok(match shared_root_of(&cwd, roots) {
            Some(root) => Decision::Say(format!(
                "git-mux: {} is shared with other agents. Commit with `git mux commit -m MSG -- PATH...` and push with `git mux push`; plain commit, stash and push, reset, clean, rebase and broad checkout are refused here. Branch work goes in a linked worktree (`git worktree add`).",
                root.path.display()
            )),
            None => Decision::Allow,
        }),
        "PostToolUse" | "PostToolUseFailure" => post(p, roots, session),
        "PermissionDenied" => {
            let key = p["tool_use_id"].as_str().unwrap_or("");
            drop_calls(roots, session, |id, _| id == key)?;
            Ok(Decision::Allow)
        }
        "Stop" | "Interrupt" => {
            drop_calls(roots, session, |_, call| call.get("agent_id").is_none())?;
            Ok(Decision::Allow)
        }
        "SubagentStop" => {
            let Some(agent) = agent else {
                return Ok(Decision::Allow);
            };
            drop_calls(roots, session, |_, call| {
                call["agent_id"].as_str() == Some(agent)
            })?;
            for root in holding(roots, session, true) {
                lease::adopt_subagent(&root.repo()?, session, agent)?;
            }
            Ok(Decision::Allow)
        }
        "SessionEnd" => {
            for root in holding(roots, session, true) {
                let repo = root.repo()?;
                {
                    let _lock = repo.lock("shell")?;
                    let _ = fs::remove_file(log_path(&repo.state, session));
                }
                lease::release_session(&repo, session)?;
            }
            Ok(Decision::Allow)
        }
        _ => pre(p, roots, &cwd, session),
    }
}

/// Roots holding state of the session: its shell log and, when `leases` is set, any lease file.
fn holding<'a>(roots: &'a [Root], session: &str, leases: bool) -> Vec<&'a Root> {
    roots
        .iter()
        .filter(|r| {
            log_path(&r.state(), session).exists()
                || (leases && r.state().join("leases.json").exists())
        })
        .collect()
}

/// Forgets the pending calls `gone` selects, so a call that will never report back stops
/// counting as a running command.
fn drop_calls(roots: &[Root], session: &str, gone: impl Fn(&str, &Value) -> bool) -> Result<()> {
    for root in holding(roots, session, false) {
        let repo = root.repo()?;
        let file = log_path(&repo.state, session);
        let _lock = repo.lock("shell")?;
        let mut log = call_log(&file);
        let Some(calls) = log["calls"].as_object_mut() else {
            continue;
        };
        let before = calls.len();
        calls.retain(|id, call| !gone(id, call));
        if calls.len() != before {
            fs::write(&file, log.to_string())?;
        }
    }
    Ok(())
}

fn post(p: &Value, roots: &[Root], session: &str) -> Result<Decision> {
    if p["tool_name"].as_str() != Some("Bash") {
        return Ok(Decision::Allow);
    }
    let mut notes = Vec::new();
    for root in holding(roots, session, false) {
        notes.extend(shell_snapshot(&root.repo()?, p, session, true)?);
    }
    Ok(if notes.is_empty() {
        Decision::Allow
    } else {
        Decision::Note(notes.join("\n"))
    })
}

fn pre(p: &Value, roots: &[Root], cwd: &Path, session: &str) -> Result<Decision> {
    let tool = p["tool_name"].as_str().unwrap_or("");
    let data = &p["tool_input"];
    let owner = Owner::from_payload(p);
    if tool == "Bash" {
        return bash(p, roots, cwd, session, &owner);
    }
    if !EDIT_TOOLS.contains(&tool) {
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
            .map(|f| cwd.join(f))
            .into_iter()
            .collect()
    };
    let mut by_root: Vec<(&Root, Vec<String>)> = Vec::new();
    for (root, rel) in files.iter().filter_map(|f| locate(f, roots)) {
        match by_root.iter_mut().find(|(r, _)| std::ptr::eq(*r, root)) {
            Some((_, paths)) => paths.push(rel),
            None => by_root.push((root, vec![rel])),
        }
    }
    for (root, paths) in by_root {
        let foreign = root.repo()?.claim(&owner, &paths)?;
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

fn bash(p: &Value, roots: &[Root], cwd: &Path, session: &str, owner: &Owner) -> Result<Decision> {
    let argv: Option<Vec<String>> = p["tool_input"]["command"].as_array().map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    });
    let command = match &argv {
        Some(a) => a.join(" "),
        None => p["tool_input"]["command"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    };
    let here = shared_root_of(cwd, roots);
    let mentioned: Vec<&Root> = roots.iter().filter(|r| r.mentioned_in(&command)).collect();
    // Where an invocation's directory is unknown, it counts as the checkout the shell is in, else
    // as any checkout the text names.
    let fallback: Vec<&Root> = here.map_or_else(|| mentioned.clone(), |r| vec![r]);
    let mut touched = fallback.clone();
    for r in mentioned {
        if !touched.iter().any(|t| std::ptr::eq(*t, r)) {
            touched.push(r);
        }
    }
    let parsed = match &argv {
        Some(a) => argv_invocations(a, cwd),
        None => invocations(&command, cwd),
    };
    let Some(parsed) = parsed else {
        if (here.is_some() || !touched.is_empty()) && names_git(&command) {
            return Ok(Decision::Deny(
                "this command cannot be parsed safely; split it into plain commands.".into(),
            ));
        }
        return snapshot_before(&touched, p, session);
    };
    let mut claims = Vec::new();
    for inv in parsed {
        let dir = inv.dir.clone().unwrap_or_else(|| cwd.to_path_buf());
        let targets: Vec<&Root> = match &inv.dir {
            Some(d) => shared_root_of(d, roots).into_iter().collect(),
            None => fallback.clone(),
        };
        for root in targets {
            if !touched.iter().any(|t| std::ptr::eq(*t, root)) {
                touched.push(root);
            }
            if inv.kind == Kind::Git {
                if let Some(why) = guard_git(&inv.args) {
                    let shown: String = inv.args.join(" ").chars().take(60).collect();
                    return Ok(Decision::Deny(format!(
                        "`git {shown}` in the shared checkout {}: {why}.",
                        root.path.display()
                    )));
                }
            } else if inv.args.first().map(String::as_str) == Some("commit")
                && !inv.args.iter().any(|a| a == "--take")
            {
                claims.push((root, dir.clone(), commit_paths(&inv.args[1..])));
            }
        }
    }
    for (root, dir, paths) in claims {
        let repo = root.repo()?;
        let Ok(paths) = expand(&repo, &paths, Some(&dir)) else {
            continue;
        };
        let held = repo.foreign_leases(owner, &paths)?;
        if !held.is_empty() {
            return Ok(Decision::Deny(format!(
                "{held} has uncommitted edits by another live agent; commit only your own paths, or pass --take when that session handed them over."
            )));
        }
    }
    snapshot_before(&touched, p, session)
}

fn snapshot_before(touched: &[&Root], p: &Value, session: &str) -> Result<Decision> {
    for root in touched {
        shell_snapshot(&root.repo()?, p, session, false)?;
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

fn log_path(state: &Path, session: &str) -> PathBuf {
    state
        .join("shell")
        .join(format!("{}.json", file_name(session)))
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
fn shell_snapshot(repo: &Repo, p: &Value, session: &str, post: bool) -> Result<Option<String>> {
    let calls = repo.state.join("shell");
    let mine = log_path(&repo.state, session);
    let key = p["tool_use_id"].as_str().unwrap_or("").to_string();
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
            // Two handlers (global and project) see the same call: the first snapshot stands.
            if key.is_empty() || !map.contains_key(&key) {
                let mut call = json!({"t0": now(), "state": tree_state(repo)?});
                if let Some(agent) = p["agent_id"].as_str() {
                    call["agent_id"] = json!(agent);
                }
                map.insert(key, call);
            }
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
    // Both ways round: a path that left the status (reverted, committed) changed too.
    let was = before["state"].as_object();
    let changed: Vec<String> = now_state
        .keys()
        .chain(was.into_iter().flat_map(|m| m.keys()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| now_state.get(*path) != was.and_then(|m| m.get(*path)))
        .cloned()
        .collect();
    if changed.is_empty() {
        return Ok(None);
    }
    // A lease ends when its path equals HEAD again; what was held when the command began still
    // counts, so a revert of another session's file is seen.
    let mut held = repo.leases()?;
    for (path, lease) in read_json(&repo.state.join("leases.json"))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
    {
        if was.is_some_and(|m| m.contains_key(&path)) {
            held.entry(path).or_insert(lease);
        }
    }
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
        .filter(|c| !noted.contains(c) && now_state.contains_key(c.as_str()))
        .cloned()
        .collect();
    if !loose.is_empty() && busy.is_empty() {
        repo.claim(&Owner::from_payload(p), &loose)?;
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
