use crate::config::{Config, expand_user};
use crate::run::{Cmd, Out, Result, refuse, zsplit};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
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
        let out = self
            .git()
            .args(["rev-parse", "-q", "--verify", &self.refname])
            .out()?;
        if !out.ok() {
            refuse!(
                "{} has no commit yet; make the first one with GIT_MUX=1 git commit --no-verify, then git mux commit works",
                self.branch
            );
        }
        Ok(out.text())
    }

    /// What uncommitted work is measured against: HEAD, or the empty tree before the first commit.
    fn baseline(&self) -> Result<String> {
        match self.head() {
            Ok(head) => Ok(head),
            Err(_) => self
                .git()
                .args(["hash-object", "-t", "tree", "/dev/null"])
                .text(),
        }
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
        let baseline = self.baseline()?;
        let mut out = BTreeSet::new();
        for chunk in path_chunks(paths) {
            out.extend(zsplit(
                &self
                    .git()
                    .args(["diff", "--name-only", "-z", &baseline, "--"])
                    .args(chunk)
                    .raw()?,
            ));
            out.extend(zsplit(
                &self
                    .git()
                    .args(["ls-files", "-z", "--others", "--exclude-standard", "--"])
                    .args(chunk)
                    .raw()?,
            ));
        }
        Ok(out)
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
        let mut entries = BTreeMap::new();
        for chunk in path_chunks(paths) {
            let out = self
                .git()
                .args(["ls-tree", "-r", "-z", treeish, "--"])
                .args(chunk)
                .raw()?;
            entries.extend(zsplit(&out).iter().filter_map(|item| {
                let (meta, path) = item.split_once('\t')?;
                let f: Vec<&str> = meta.split_whitespace().collect();
                Some((
                    path.to_string(),
                    (f.first()?.to_string(), f.get(2)?.to_string()),
                ))
            }));
        }
        Ok(entries)
    }

    pub fn index_entries(
        &self,
        paths: &[String],
        index: Option<&Path>,
    ) -> Result<BTreeMap<String, Entry>> {
        let mut entries = BTreeMap::new();
        for chunk in path_chunks(paths) {
            let mut c = self.git();
            if let Some(i) = index {
                c = c.env("GIT_INDEX_FILE", i);
            }
            let out = c.args(["ls-files", "-s", "-z", "--"]).args(chunk).raw()?;
            entries.extend(zsplit(&out).iter().filter_map(|item| {
                let (meta, path) = item.split_once('\t')?;
                let f: Vec<&str> = meta.split_whitespace().collect();
                (f.get(2) == Some(&"0"))
                    .then(|| (path.to_string(), (f[0].to_string(), f[1].to_string())))
            }));
        }
        Ok(entries)
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

    /// Runs `git update-index` on `input` while someone else holds index.lock, which clears by
    /// itself within moments.
    pub fn update_index(&self, args: &[&str], input: Vec<u8>) -> Result<Out> {
        for _ in 0..50 {
            let r = self
                .git()
                .arg("update-index")
                .args(args)
                .input(input.clone())
                .out()?;
            if r.ok() || !r.stderr.contains("index.lock") {
                return Ok(r);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        refuse!("the shared index stayed locked; the next git-mux run updates it");
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
                    lines.push_str(&format!("{mode} {oid}\t{p}\0"));
                } else {
                    kept.push(p.clone());
                }
            }
            let r = self.update_index(&["-z", "--index-info"], lines.into_bytes())?;
            if !r.ok() {
                eprintln!(
                    "git-mux: the shared index was not updated: {}",
                    r.stderr.trim()
                );
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

/// Argument lists for git calls that take paths on the command line: the arguments of one call
/// stay under 64 KiB, far below what the operating system accepts.
pub fn path_chunks(paths: &[String]) -> Vec<&[String]> {
    const LIMIT: usize = 64 * 1024;
    let (mut chunks, mut start, mut size) = (Vec::new(), 0, 0);
    for (i, p) in paths.iter().enumerate() {
        if size > 0 && size + p.len() + 1 > LIMIT {
            chunks.push(&paths[start..i]);
            (start, size) = (i, 0);
        }
        size += p.len() + 1;
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

pub fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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
