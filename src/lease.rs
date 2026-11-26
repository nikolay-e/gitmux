use crate::repo::{Repo, now, read_json, write_atomic};
use crate::run::{Cmd, Result, refuse};
use serde_json::{Map, Value, json};
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// Who is acting: the agent session, for a subagent its id, and the agent process that hosts both.
pub struct Owner {
    session: Option<String>,
    ambient: bool,
    agent: Option<String>,
    process: OnceCell<Found>,
}

/// The nearest agent ancestor: its recorded identity and the agent it matched in the list.
struct Found {
    record: Map<String, Value>,
    agent: Option<String>,
}

impl Owner {
    pub fn from_payload(p: &Value) -> Owner {
        let text = |k: &str| p[k].as_str().map(String::from);
        Owner::new(text("session_id"), text("agent_id"), false)
    }

    /// The session comes from the environment, which every descendant inherits: it counts only
    /// when no agent but Claude Code stands between this process and that session's shell.
    pub fn of_process() -> Owner {
        Owner::new(std::env::var("CLAUDE_CODE_SESSION_ID").ok(), None, true)
    }

    fn new(session: Option<String>, agent: Option<String>, ambient: bool) -> Owner {
        Owner {
            session,
            ambient,
            agent: agent.filter(|a| !a.is_empty()),
            process: OnceCell::new(),
        }
    }

    fn found(&self, agents: &[String]) -> &Found {
        self.process.get_or_init(|| nearest_agent(agents))
    }

    fn session_id(&self, agents: &[String]) -> Option<&str> {
        let nearer_agent = self.ambient
            && self
                .found(agents)
                .agent
                .as_deref()
                .is_some_and(|a| a != "claude");
        self.session.as_deref().filter(|_| !nearer_agent)
    }

    fn session(&self, agents: &[String]) -> &str {
        self.session_id(agents).unwrap_or("unknown")
    }

    fn process(&self, agents: &[String]) -> &Map<String, Value> {
        &self.found(agents).record
    }

    /// Why the lease is another owner's: a different session or two different subagents of one
    /// session. A caller with no session is judged by agent process alone: it is the holder only
    /// when both have a recorded process and it is the same one.
    fn clash(&self, lease: &Value, agents: &[String]) -> Option<String> {
        let held = lease.get("session")?.as_str()?;
        let Some(session) = self.session_id(agents) else {
            let recorded = |m: &Map<String, Value>| {
                Some((
                    m.get("pid")?.as_i64()?,
                    m.get("start")?.as_str()?.to_string(),
                ))
            };
            let same = lease
                .as_object()
                .and_then(recorded)
                .zip(recorded(self.process(agents)))
                .is_some_and(|(theirs, mine)| theirs == mine);
            return (!same).then(|| held.to_string());
        };
        if held != session {
            return Some(held.to_string());
        }
        if let (Some(mine), Some(theirs)) =
            (&self.agent, lease.get("agent").and_then(Value::as_str))
            && !theirs.is_empty()
            && mine != theirs
        {
            return Some(format!("{held} (subagent {})", short(theirs)));
        }
        None
    }

    /// The holder of a live lease that is not this owner's.
    fn conflict(&self, lease: &Value, agents: &[String]) -> Option<String> {
        self.clash(lease, agents).filter(|_| alive(lease))
    }
}

impl Repo {
    pub fn leases(&self) -> Result<Map<String, Value>> {
        let held = read_json(&self.state.join("leases.json"))
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let live = self.dirty(&held.keys().cloned().collect::<Vec<_>>())?;
        Ok(held.into_iter().filter(|(p, _)| live.contains(p)).collect())
    }

    fn save_leases(&self, held: &Map<String, Value>) -> Result<()> {
        write_atomic(
            &self.state.join("leases.json"),
            serde_json::to_string_pretty(held).unwrap().as_bytes(),
            None,
        )
    }

    /// Claims every unowned or orphaned path for the owner, marks when it last touched each, and
    /// returns those another live owner holds with their holder.
    pub fn claim(&self, owner: &Owner, paths: &[String]) -> Result<BTreeMap<String, String>> {
        let agents = &self.cfg.agents;
        let _lock = self.lock("leases")?;
        let mut held = self.leases()?;
        let mut foreign = BTreeMap::new();
        for p in paths {
            if let Some(who) = held.get(p).and_then(|l| owner.conflict(l, agents)) {
                foreign.insert(p.clone(), who);
            }
        }
        let mine: Vec<&String> = paths.iter().filter(|p| !foreign.contains_key(*p)).collect();
        if mine.is_empty() {
            return Ok(foreign);
        }
        let now = now();
        for p in mine {
            if held.get(p).is_none_or(|l| owner.clash(l, agents).is_some()) {
                let mut lease = json!({"session": owner.session(agents), "since": now as i64});
                let fields = lease.as_object_mut().unwrap();
                fields.extend(owner.process(agents).clone());
                if let Some(agent) = &owner.agent {
                    fields.insert("agent".into(), json!(agent));
                }
                held.insert(p.clone(), lease);
            }
            held[p.as_str()]["touched"] = json!(now);
        }
        self.save_leases(&held)?;
        Ok(foreign)
    }

    pub fn foreign_leases(&self, owner: &Owner, paths: &[String]) -> Result<String> {
        let held = self.leases()?;
        let foreign: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                let who = owner.conflict(held.get(p)?, &self.cfg.agents)?;
                Some(format!("{p} (session {})", short(&who)))
            })
            .collect();
        Ok(foreign.join(", "))
    }

    /// Rewrites the leases under the lock, saving only when `edit` changed something.
    fn edit_leases(&self, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
        let _lock = self.lock("leases")?;
        let mut held = self.leases()?;
        let before = held.clone();
        edit(&mut held);
        if held != before {
            self.save_leases(&held)?;
        }
        Ok(())
    }
}

pub fn holder(held: &Map<String, Value>, path: &str) -> Option<String> {
    held.get(path)?.get("session")?.as_str().map(String::from)
}

pub fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

type Processes = HashMap<i64, (i64, String, String)>;

/// pid → (parent pid, start time, command name); None when `ps` cannot be read. One listing per
/// process serves both the agent ancestry and every liveness check.
fn processes() -> Option<&'static Processes> {
    static TABLE: OnceLock<Option<Processes>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let out = Cmd::new("ps")
                .args(["-A", "-o", "pid=,ppid=,lstart=,comm="])
                .out()
                .ok()?;
            let mut table = HashMap::new();
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 8 {
                    continue;
                }
                let (Ok(pid), Ok(ppid)) = (f[0].parse(), f[1].parse()) else {
                    continue;
                };
                let comm = f[7..].join(" ");
                let name = comm.rsplit('/').next().unwrap_or(&comm).to_string();
                table.insert(pid, (ppid, f[2..7].join(" "), name));
            }
            Some(table)
        })
        .as_ref()
}

/// pid → the script a `node` process runs, as a basename without its extension: the first
/// argument after node's own options. Read only when an ancestor is `node`.
fn node_scripts() -> &'static HashMap<i64, String> {
    static TABLE: OnceLock<HashMap<i64, String>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let Ok(out) = Cmd::new("ps").args(["-A", "-o", "pid=,args="]).out() else {
            return HashMap::new();
        };
        let mut table = HashMap::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut words = line.split_whitespace();
            let Some(Ok(pid)) = words.next().map(str::parse) else {
                continue;
            };
            let Some(script) = words.skip(1).find(|w| !w.starts_with('-')) else {
                continue;
            };
            let file = script.rsplit('/').next().unwrap_or(script);
            let stem = [".js", ".mjs", ".cjs"]
                .iter()
                .find_map(|ext| file.strip_suffix(ext))
                .unwrap_or(file);
            table.insert(pid, stem.to_string());
        }
        table
    })
}

fn nearest_agent(agents: &[String]) -> Found {
    let none = Found {
        record: Map::new(),
        agent: None,
    };
    let Some(table) = processes() else {
        return none;
    };
    let named = |name: &str| agents.iter().find(|a| *a == name);
    let mut pid = std::os::unix::process::parent_id() as i64;
    while pid > 1 {
        let Some((ppid, start, name)) = table.get(&pid) else {
            break;
        };
        let agent = named(name).or_else(|| {
            let script = (name == "node").then(|| node_scripts().get(&pid))??;
            named(script)
        });
        if let Some(agent) = agent {
            let mut record = Map::new();
            record.insert("pid".into(), json!(pid));
            record.insert("start".into(), json!(start));
            return Found {
                record,
                agent: Some(agent.clone()),
            };
        }
        pid = *ppid;
    }
    none
}

/// A lease records the agent process that took it, so a crashed or closed session's files free
/// themselves: the holder is gone once no process has that pid with that start time.
pub fn agent_process(agents: &[String]) -> Map<String, Value> {
    nearest_agent(agents).record
}

/// A holder with no recorded process, or a machine whose process table cannot be read, counts as
/// alive: treating it as gone would hand an unidentified agent's files to other sessions.
pub fn alive(lease: &Value) -> bool {
    let Some(pid) = lease.get("pid").and_then(Value::as_i64).filter(|p| *p > 0) else {
        return true;
    };
    let Some(table) = processes() else {
        return true;
    };
    let start = lease.get("start").and_then(Value::as_str).unwrap_or("");
    table.get(&pid).is_some_and(|(_, s, _)| s == start)
}

/// Refuses paths another live owner holds, unless the caller was told to take them over.
pub fn refuse_foreign(repo: &Repo, paths: &[String], owner: &Owner, take: bool) -> Result<()> {
    if take {
        return Ok(());
    }
    let held = repo.foreign_leases(owner, paths)?;
    if !held.is_empty() {
        refuse!(
            "{held} has uncommitted edits by another live agent; commit only your own paths, or pass --take when that session handed them over"
        );
    }
    Ok(())
}

/// A session that ended holds nothing.
pub fn release_session(repo: &Repo, session: &str) -> Result<()> {
    repo.edit_leases(|held| held.retain(|_, lease| lease["session"] != session))
}

/// A finished subagent hands its files to the session it worked for.
pub fn adopt_subagent(repo: &Repo, session: &str, agent: &str) -> Result<()> {
    repo.edit_leases(|held| {
        for lease in held.values_mut() {
            if lease["session"] == session
                && lease["agent"] == agent
                && let Some(fields) = lease.as_object_mut()
            {
                fields.remove("agent");
            }
        }
    })
}
