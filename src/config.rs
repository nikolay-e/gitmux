use crate::run::Cmd;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Settings live in git config: the repository's own `.git/config` wins over a committed
/// `.gitmux` file in git-config syntax, which carries the team's defaults. `selfInstall` replaces
/// the running binary, so only the local config may set it.
pub struct Config {
    pub slots: usize,
    pub slot_dir: PathBuf,
    pub agents: Vec<String>,
    pub components: String,
    pub self_install: Option<String>,
    pub slot_setup: Option<String>,
}

enum Source<'a> {
    All,
    Local,
    File(&'a Path),
}

fn read(root: &Path, source: Source) -> HashMap<String, Vec<String>> {
    let mut c = Cmd::git(root).arg("config");
    c = match source {
        Source::All => c,
        Source::Local => c.arg("--local"),
        Source::File(f) => c.arg("-f").arg(f),
    };
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(out) = c.args(["-z", "--get-regexp", r"^gitmux\."]).out() {
        for item in String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
        {
            let (key, value) = item.split_once('\n').unwrap_or((item, ""));
            map.entry(key.to_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }
    map
}

pub fn expand_user(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ if path == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.into()),
        _ => PathBuf::from(path),
    }
}

/// `${XDG_CACHE_HOME:-~/.cache}/gitmux/slots/<name>-<8 hex of the common dir's hash>`, outside
/// the repository, one per repository.
fn default_slot_dir(root: &Path, common: &Path) -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))?;
    let name = match common.file_name() {
        Some(n) if n == ".git" => common.parent().and_then(Path::file_name),
        other => other,
    }?;
    let hash = Cmd::git(root)
        .args(["hash-object", "--stdin"])
        .input(common.to_string_lossy().into_owned())
        .text()
        .ok()?;
    Some(
        cache
            .join("gitmux/slots")
            .join(format!("{}-{}", name.to_string_lossy(), hash.get(..8)?)),
    )
}

impl Config {
    pub fn load(root: &Path, state: &Path) -> Config {
        let file = root.join(".gitmux");
        let shared = if file.is_file() {
            read(root, Source::File(&file))
        } else {
            HashMap::new()
        };
        let configured = read(root, Source::All);
        let get = |k: &str| {
            configured
                .get(k)
                .or_else(|| shared.get(k))
                .cloned()
                .unwrap_or_default()
        };
        let one = |k: &str| get(k).pop().filter(|v| !v.is_empty());
        let agents = get("gitmux.agent");
        Config {
            slots: one("gitmux.slots")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(2),
            slot_dir: one("gitmux.slotdir")
                .map(|d| root.join(expand_user(&d)))
                .or_else(|| default_slot_dir(root, state.parent().unwrap_or(state)))
                .unwrap_or_else(|| state.join("slots")),
            agents: if agents.is_empty() {
                vec!["claude".into(), "codex".into(), "gemini".into()]
            } else {
                agents
            },
            components: one("gitmux.components").unwrap_or_else(|| "components.json".into()),
            self_install: read(root, Source::Local)
                .get("gitmux.selfinstall")
                .and_then(|v| v.last())
                .filter(|v| !v.is_empty())
                .map(|p| p.trim_end_matches('/').to_string()),
            slot_setup: one("gitmux.slotsetup"),
        }
    }
}
