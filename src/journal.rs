use crate::repo::{Repo, read_json, strs, unique};
use crate::run::Result;
use crate::sync::bring_tree;
use serde_json::json;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// A publish moves the branch first and the shared index (and, for sync, the files) after. A
/// process killed or refused in between leaves its journal behind, locked by nobody: the next
/// git-mux run finishes the second half, for the paths that still hold the old state.
pub struct Journal {
    path: PathBuf,
    _file: File,
}

impl Journal {
    pub fn open(
        repo: &Repo,
        kind: &str,
        old: &str,
        new: &str,
        paths: &[String],
    ) -> Result<Journal> {
        let tmp = unique(&repo.state, "journal-");
        let mut f = File::create(&tmp)?;
        f.lock()?;
        f.write_all(
            json!({"kind": kind, "old": old, "new": new, "paths": paths})
                .to_string()
                .as_bytes(),
        )?;
        f.flush()?;
        let path = repo
            .state
            .join(format!("publish.{}.json", std::process::id()));
        fs::rename(&tmp, &path)?;
        Ok(Journal { path, _file: f })
    }

    pub fn finish(self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn heal(repo: &Repo) -> Result<()> {
    for entry in fs::read_dir(&repo.state)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !(name.starts_with("publish.") && name.ends_with(".json")) {
            continue;
        }
        let Ok(f) = File::open(&path) else { continue };
        if f.try_lock().is_err() || !path.exists() {
            continue;
        }
        let Some(j) = read_json(&path) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let (kind, old, new) = (
            j["kind"].as_str().unwrap_or(""),
            j["old"].as_str().unwrap_or(""),
            j["new"].as_str().unwrap_or(""),
        );
        let paths = strs(&j["paths"]);
        let landed = repo
            .git()
            .args(["merge-base", "--is-ancestor", new, &repo.head()?])
            .out()?
            .ok();
        if landed && kind == "commit" {
            repo.sync_index(old, new, &paths)?;
        } else if landed {
            let before = repo.entries(old, &paths)?;
            let mut stale = Vec::new();
            for p in &paths {
                let live = fs::read(repo.root.join(p)).ok();
                let was = before.get(p).map(|(_, oid)| repo.blob(oid)).transpose()?;
                if live == was {
                    stale.push(p.clone());
                }
            }
            bring_tree(repo, new, &stale)?;
        }
        let _ = fs::remove_file(&path);
        eprintln!(
            "git-mux: finished the interrupted {kind} of {}",
            new.get(..10).unwrap_or(new)
        );
    }
    Ok(())
}
