use crate::repo::{Repo, now, read_json, write_atomic};
use crate::run::{Cmd, Result, refuse};
use serde_json::{Map, Value, json};
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// Who is acting: the agent session, for a subagent its id, and the agent process that hosts both.
pub struct Owner {
    session: Option<String>,
    agent: Option<String>,
    process: OnceCell<Map<String, Value>>,
}

impl Owner {
    pub fn from_payload(p: &Value) -> Owner {
        let text = |k: &str| p[k].as_str().map(String::from);
        Owner::new(text("session_id"), text("agent_id"))
    }

    #[allow(dead_code)]
    pub fn of_process() -> Owner {
        Owner::new(std::env::var("CLAUDE_CODE_SESSION_ID").ok(), None)
    }

    fn new(session: Option<String>, agent: Option<String>) -> Owner {
        Owner {
            session,
            agent: agent.filter(|a| !a.is_empty()),
            process: OnceCell::new(),
        }
    }

    fn session(&self) -> &str {
        self.session.as_deref().unwrap_or("unknown")
    }

    fn process(&self, agents: &[String]) -> &Map<String, Value> {
        self.process.get_or_init(|| agent_process(agents))
    }

    /// Why the lease is another owner's: a different session, two different subagents of one
    /// session, or, when this caller has no session, a different agent process.
    fn clash(&self, lease: &Value, agents: &[String]) -> Option<String> {
        let held = lease.get("session")?.as_str()?;
        if held != self.session() {
            return Some(held.to_string());
        }
        if let (Some(mine), Some(theirs)) =
            (&self.agent, lease.get("agent").and_then(Value::as_str))
            && !theirs.is_empty()
            && mine != theirs
        {
            return Some(format!("{held} (subagent {})", short(theirs)));
        }
        let recorded = |m: &Map<String, Value>| {
            Some((
                m.get("pid")?.as_i64()?,
                m.get("start")?.as_str()?.to_string(),
            ))
        };
        if self.session.is_none()
            && let (Some(theirs), Some(mine)) = (
                lease.as_object().and_then(recorded),
                recorded(self.process(agents)),
            )
            && theirs != mine
        {
            return Some(format!("{held} (process {})", theirs.0));
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
                let mut lease = json!({"session": owner.session(), "since": now as i64});
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

/// A lease records the agent process that took it, so a crashed or closed session's files free
/// themselves: the holder is gone once no process has that pid with that start time.
pub fn agent_process(agents: &[String]) -> Map<String, Value> {
    let Some(table) = processes() else {
        return Map::new();
    };
    let mut pid = std::os::unix::process::parent_id() as i64;
    while pid > 1 {
        let Some((ppid, start, name)) = table.get(&pid) else {
            break;
        };
        if agents.iter().any(|a| a == name) {
            let mut m = Map::new();
            m.insert("pid".into(), json!(pid));
            m.insert("start".into(), json!(start));
            return m;
        }
        pid = *ppid;
    }
    Map::new()
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn release_session(repo: &Repo, session: &str) -> Result<()> {
    repo.edit_leases(|held| held.retain(|_, lease| lease["session"] != session))
}

/// A finished subagent hands its files to the session it worked for.
#[allow(dead_code)]
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
