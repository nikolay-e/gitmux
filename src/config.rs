use crate::run::Cmd;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Settings live in git config: the repository's own `.git/config` wins over a committed
/// `.gitmux` file in git-config syntax, which carries the team's defaults.
pub struct Config {
    pub slots: usize,
    pub slot_dir: PathBuf,
    pub agents: Vec<String>,
    pub components: String,
    pub self_install: Option<String>,
}

fn read(root: &Path, file: Option<&Path>) -> HashMap<String, Vec<String>> {
    let mut c = Cmd::git(root).arg("config");
    if let Some(f) = file {
        c = c.arg("-f").arg(f);
    }
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

impl Config {
    pub fn load(root: &Path, state: &Path) -> Config {
        let file = root.join(".gitmux");
        let shared = if file.is_file() {
            read(root, Some(&file))
        } else {
            HashMap::new()
        };
        let local = read(root, None);
        let get = |k: &str| {
            local
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
                .unwrap_or_else(|| state.join("slots")),
            agents: if agents.is_empty() {
                vec!["claude".into(), "codex".into()]
            } else {
                agents
            },
            components: one("gitmux.components").unwrap_or_else(|| "components.json".into()),
            self_install: one("gitmux.selfinstall").map(|p| p.trim_end_matches('/').to_string()),
        }
    }
}
