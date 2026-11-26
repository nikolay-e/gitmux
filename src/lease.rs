use crate::repo::{Repo, now, read_json, write_atomic};
use crate::run::{Cmd, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};

/// Who is acting: the agent session and, for a subagent, its id.
pub struct Owner {
    session: Option<String>,
    #[allow(dead_code)]
    agent: Option<String>,
}

impl Owner {
    pub fn from_payload(p: &Value) -> Owner {
        let text = |k: &str| p[k].as_str().map(String::from);
        Owner {
            session: text("session_id"),
            agent: text("agent_id"),
        }
    }

    #[allow(dead_code)]
    pub fn of_process() -> Owner {
        Owner {
            session: std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
            agent: None,
        }
    }

    fn session(&self) -> &str {
        self.session.as_deref().unwrap_or("unknown")
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

    /// Claims every unowned or orphaned path for the session, marks when it last touched each, and
    /// returns those a live session holds.
    pub fn claim(&self, owner: &Owner, paths: &[String]) -> Result<BTreeMap<String, String>> {
        let session = owner.session();
        let _lock = self.lock("leases")?;
        let mut held = self.leases()?;
        let mut foreign = BTreeMap::new();
        for p in paths {
            if let Some(s) = holder(&held, p)
                && s != session
                && alive(&held[p])
            {
                foreign.insert(p.clone(), s);
            }
        }
        let mine: Vec<&String> = paths.iter().filter(|p| !foreign.contains_key(*p)).collect();
        if mine.is_empty() {
            return Ok(foreign);
        }
        let now = now();
        let mut me: Option<Value> = None;
        for p in mine {
            if holder(&held, p).as_deref() != Some(session) {
                let lease = me.get_or_insert_with(|| {
                    let mut l = json!({"session": session, "since": now as i64});
                    l.as_object_mut()
                        .unwrap()
                        .extend(agent_process(&self.cfg.agents));
                    l
                });
                held.insert(p.clone(), lease.clone());
            }
            held[p.as_str()]["touched"] = json!(now);
        }
        self.save_leases(&held)?;
        Ok(foreign)
    }

    pub fn foreign_leases(&self, owner: &Owner, paths: &[String]) -> Result<String> {
        let session = owner.session();
        let held = self.leases()?;
        let foreign: Vec<String> = paths
            .iter()
            .filter_map(|p| {
                holder(&held, p)
                    .filter(|s| s != session && alive(&held[p.as_str()]))
                    .map(|s| format!("{p} (session {})", short(&s)))
            })
            .collect();
        Ok(foreign.join(", "))
    }
}

pub fn holder(held: &Map<String, Value>, path: &str) -> Option<String> {
    held.get(path)?.get("session")?.as_str().map(String::from)
}

pub fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

fn ps_start(pid: i64) -> Option<String> {
    let o = Cmd::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .out()
        .ok()?;
    Some(
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// A lease records the agent process that took it, so a crashed or closed session's files free
/// themselves: the holder is gone once no process has that pid with that start time.
pub fn agent_process(agents: &[String]) -> Map<String, Value> {
    let Ok(out) = Cmd::new("ps")
        .args(["-A", "-o", "pid=,ppid=,lstart=,comm="])
        .out()
    else {
        return Map::new();
    };
    let mut table: HashMap<i64, (i64, String, String)> = HashMap::new();
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

pub fn alive(lease: &Value) -> bool {
    let Some(pid) = lease.get("pid").and_then(Value::as_i64).filter(|p| *p > 0) else {
        return true;
    };
    let start = lease.get("start").and_then(Value::as_str).unwrap_or("");
    ps_start(pid).is_none_or(|s| s == start)
}

#[allow(dead_code, unused_variables)]
pub fn refuse_foreign(repo: &Repo, paths: &[String], owner: &Owner, take: bool) -> Result<()> {
    Ok(())
}

#[allow(dead_code, unused_variables)]
pub fn release_session(repo: &Repo, session: &str) -> Result<()> {
    Ok(())
}

#[allow(dead_code, unused_variables)]
pub fn adopt_subagent(repo: &Repo, session: &str, agent: &str) -> Result<()> {
    Ok(())
}
