use crate::config::{Config, expand_user};
use crate::run::{Cmd, Result, refuse, zsplit};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// (mode, object id) of one path in a tree or index.
pub type Entry = (String, String);

pub struct Repo {
    pub root: PathBuf,
    pub common: PathBuf,
    pub state: PathBuf,
    pub refname: String,
    pub branch: String,
    pub cfg: Config,
}

impl Repo {
    pub fn open(dir: &Path) -> Result<Repo> {
        let root = PathBuf::from(
            Cmd::git(dir)
                .args(["rev-parse", "--show-toplevel"])
                .text()?,
        );
        let common = PathBuf::from(
            Cmd::git(&root)
                .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .text()?,
        );
        let state = common.join("gitmux");
        let legacy = common.join("mux");
        if !state.exists() && legacy.is_dir() {
            let _ = fs::rename(&legacy, &state);
        }
        fs::create_dir_all(&state)?;
        let refname = Cmd::git(&root)
            .args(["symbolic-ref", "-q", "HEAD"])
            .out()?
            .text();
        if refname.is_empty() {
            refuse!(
                "{} has a detached HEAD; git-mux commits to the checked-out branch",
                root.display()
            );
        }
        let branch = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(&refname)
            .to_string();
        let cfg = Config::load(&root, &state);
        Ok(Repo {
            root,
            common,
            state,
            refname,
            branch,
            cfg,
        })
    }

    /// The main checkout: the one every agent shares and the hooks guard.
    pub fn main_root(&self) -> PathBuf {
        match self.common.file_name() {
            Some(n) if n == ".git" => self.common.parent().unwrap_or(&self.root).to_path_buf(),
            _ => self.root.clone(),
        }
    }

    pub fn git(&self) -> Cmd {
        Cmd::git(&self.root)
    }

    pub fn head(&self) -> Result<String> {
        self.git().args(["rev-parse", &self.refname]).text()
    }

    pub fn lock(&self, name: &str) -> Result<File> {
        let f = File::create(self.state.join(format!("{name}.lock")))?;
        f.lock()?;
        Ok(f)
    }

    /// A lease lives exactly as long as its path differs from HEAD: committing or reverting the
    /// file ends it, so nothing expires on a clock.
    pub fn dirty(&self, paths: &[String]) -> Result<BTreeSet<String>> {
        if paths.is_empty() {
            return Ok(BTreeSet::new());
        }
        let mut out: BTreeSet<String> = zsplit(
            &self
                .git()
                .args(["diff", "--name-only", "-z", "HEAD", "--"])
                .args(paths)
                .raw()?,
        )
        .into_iter()
        .collect();
        out.extend(zsplit(
            &self
                .git()
                .args(["ls-files", "-z", "--others", "--exclude-standard", "--"])
                .args(paths)
                .raw()?,
        ));
        Ok(out)
    }

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
    pub fn claim(&self, session: &str, paths: &[String]) -> Result<BTreeMap<String, String>> {
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

    pub fn foreign_leases(&self, session: &str, paths: &[String]) -> Result<String> {
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

    /// The path relative to the checkout, or None when it lies outside.
    pub fn rel(&self, path: &str, base: Option<&Path>) -> Option<String> {
        let p = expand_user(path);
        let p = if p.is_absolute() {
            p
        } else {
            base.map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok())?
                .join(p)
        };
        let p = normalize(&p);
        if let Ok(r) = p.strip_prefix(&self.root) {
            return Some(posix(r));
        }
        let root = fs::canonicalize(&self.root).ok()?;
        resolve(&p).strip_prefix(&root).ok().map(posix)
    }

    pub fn entries(&self, treeish: &str, paths: &[String]) -> Result<BTreeMap<String, Entry>> {
        if paths.is_empty() {
            return Ok(BTreeMap::new());
        }
        let out = self
            .git()
            .args(["ls-tree", "-r", "-z", treeish, "--"])
            .args(paths)
            .raw()?;
        Ok(zsplit(&out)
            .iter()
            .filter_map(|item| {
                let (meta, path) = item.split_once('\t')?;
                let f: Vec<&str> = meta.split_whitespace().collect();
                Some((
                    path.to_string(),
                    (f.first()?.to_string(), f.get(2)?.to_string()),
                ))
            })
            .collect())
    }

    pub fn index_entries(
        &self,
        paths: &[String],
        index: Option<&Path>,
    ) -> Result<BTreeMap<String, Entry>> {
        if paths.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut c = self.git();
        if let Some(i) = index {
            c = c.env("GIT_INDEX_FILE", i);
        }
        let out = c.args(["ls-files", "-s", "-z", "--"]).args(paths).raw()?;
        Ok(zsplit(&out)
            .iter()
            .filter_map(|item| {
                let (meta, path) = item.split_once('\t')?;
                let f: Vec<&str> = meta.split_whitespace().collect();
                (f.get(2) == Some(&"0"))
                    .then(|| (path.to_string(), (f[0].to_string(), f[1].to_string())))
            })
            .collect())
    }

    pub fn commit_tree(
        &self,
        tree: &str,
        parent: &str,
        message: &str,
        env: &[(&str, String)],
    ) -> Result<String> {
        let sign = self
            .git()
            .args(["config", "--bool", "--default", "false", "commit.gpgsign"])
            .text()?
            == "true";
        let mut c = self.git().arg("commit-tree");
        if sign {
            c = c.arg("-S");
        }
        for (k, v) in env {
            c = c.env(k, v);
        }
        c.args([tree, "-p", parent]).input(message).text()
    }

    pub fn blob(&self, oid: &str) -> Result<Vec<u8>> {
        self.git().args(["cat-file", "blob", oid]).bytes()
    }

    /// The shared index is everyone's: a committed path's entry moves to the new blob only if it
    /// still held the old one (or already the new one). Anything else is someone's staging.
    pub fn sync_index(&self, base: &str, commit: &str, paths: &[String]) -> Result<()> {
        let (old, new) = (self.entries(base, paths)?, self.entries(commit, paths)?);
        let zero = "0".repeat(commit.len());
        let mut kept = Vec::new();
        {
            let _lock = self.lock("index")?;
            let cur = self.index_entries(paths, None)?;
            let mut lines = String::new();
            for p in paths {
                if cur.get(p) == old.get(p) || cur.get(p) == new.get(p) {
                    let (mode, oid) = new.get(p).cloned().unwrap_or(("0".into(), zero.clone()));
                    lines.push_str(&format!("{mode} {oid}\t{p}\n"));
                } else {
                    kept.push(p.clone());
                }
            }
            let mut attempt = 0;
            loop {
                let r = self
                    .git()
                    .args(["update-index", "--index-info"])
                    .input(lines.clone())
                    .out()?;
                if r.ok() || !r.stderr.contains("index.lock") {
                    break;
                }
                attempt += 1;
                if attempt == 50 {
                    refuse!("the shared index stayed locked; the next git-mux run updates it");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = self.git().args(["update-index", "-q", "--refresh"]).out();
        }
        for p in kept {
            println!(
                "git-mux: left the shared index entry of {p} alone (someone staged a different version)"
            );
        }
        Ok(())
    }

    pub fn upstream_remote(&self) -> String {
        self.git()
            .args(["config", &format!("branch.{}.remote", self.branch)])
            .out()
            .map(|o| o.text())
            .ok()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "origin".into())
    }
}

pub fn holder(held: &Map<String, Value>, path: &str) -> Option<String> {
    held.get(path)?.get("session")?.as_str().map(String::from)
}

pub fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn unique(dir: &Path, prefix: &str) -> PathBuf {
    dir.join(format!(
        "{prefix}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A file path removed when dropped.
pub struct TempPath(pub PathBuf);

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn write_atomic(path: &Path, data: &[u8], mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = unique(
        path.parent().unwrap_or(Path::new(".")),
        &format!(".{name}.gitmux-"),
    );
    fs::write(&tmp, data)?;
    if let Some(m) = mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(m))?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Like Python's `Path.resolve()`: symlinks in the part that exists are resolved, the rest kept.
pub fn resolve(p: &Path) -> PathBuf {
    let mut head = p.to_path_buf();
    let mut tail = Vec::new();
    loop {
        if let Ok(real) = fs::canonicalize(&head) {
            return tail
                .iter()
                .rev()
                .fold(real, |acc, c: &std::ffi::OsString| acc.join(c));
        }
        match (head.file_name().map(|n| n.to_os_string()), head.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                head = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

pub fn posix(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if s.is_empty() { ".".into() } else { s }
}

pub fn inside(path: &Path, root: &Path) -> bool {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolve(&normalize(path)).starts_with(root)
}
