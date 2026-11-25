use crate::repo::{Repo, write_atomic};
use crate::run::{Cmd, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const HEADER: &str = "#!/bin/sh\n# Installed by git-mux (https://github.com/nikolay-e/gitmux); rewritten on every git-mux run.\n";

const GUARD: &str = r#"# In the shared checkout a commit, stash or push made outside git-mux stashes or sweeps in other
# sessions' work, so git refuses it here. Linked worktrees are not guarded. GIT_MUX=1 overrides.
[ -n "$GIT_MUX" ] && exit 0
[ "$(git rev-parse --absolute-git-dir)" = "$(git rev-parse --path-format=absolute --git-common-dir)" ] || exit 0
echo "git-mux: plain git $1 in the shared checkout $(git rev-parse --show-toplevel) is refused." \
  "Commit with: git mux commit -m MSG -- PATH...; push with: git mux push" >&2
exit 1
"#;

/// The shared branch moves only through git-mux, from any worktree, except a fast-forward to
/// commits a remote already has (a plain pull sweeps in nothing local); gc re-packs a ref at its
/// own value. A new stash is refused in the shared checkout only. HEAD is left to the tool hook:
/// a branch switch and a new worktree's HEAD look alike here, and a switch rewrites files first.
const REF_GUARD: &str = r#"[ "$1" = prepared ] && [ -z "$GIT_MUX" ] || exit 0
zero=0000000000000000000000000000000000000000
common=$(git rev-parse --path-format=absolute --git-common-dir)
branch=$(git --git-dir="$common" symbolic-ref -q HEAD)
while read -r old new ref; do
  [ "$new" = "$zero" ] && continue
  case "$ref" in
    "$branch") ;;
    refs/stash) [ "$(git rev-parse --absolute-git-dir)" = "$common" ] || continue ;;
    *) continue ;;
  esac
  [ "$(git rev-parse -q --verify "$ref")" = "$new" ] && continue
  if [ "$ref" = "$branch" ] && [ "$old" != "$zero" ] && git merge-base --is-ancestor "$old" "$new" &&
    [ -n "$(git for-each-ref --count=1 --contains "$new" refs/remotes)" ]; then
    continue
  fi
  echo "git-mux: refused to move $ref outside git-mux; it holds every session's work in $(dirname "$common")." \
    "Commit with: git mux commit -m MSG -- PATH...; or set GIT_MUX=1 for one deliberate command." >&2
  exit 1
done
"#;

fn registry() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
        .unwrap_or_default();
    base.join("gitmux").join("repos")
}

/// The checkouts whose agents the hook coordinates; every git-mux run in one enrolls it.
pub fn registered_roots() -> Vec<PathBuf> {
    fs::read_to_string(registry())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn put(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let same = fs::read(path).is_ok_and(|d| d == text.as_bytes())
        && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if !same {
        write_atomic(path, text.as_bytes(), Some(0o755))?;
    }
    Ok(())
}

/// Git itself refuses the other ways in, for any process: a reference-transaction hook and a
/// guard in front of the pre-commit and pre-push hooks.
pub fn ensure(repo: &Repo) -> Result<()> {
    let hooks = PathBuf::from(
        repo.git()
            .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
            .text()?,
    );
    fs::create_dir_all(&hooks)?;
    if repo.root.join(".pre-commit-config.yaml").is_file() && !hooks.join("pre-commit").exists() {
        let _ = Cmd::new("pre-commit").arg("install").cwd(&repo.root).out();
    }
    put(&hooks.join("mux-guard"), &format!("{HEADER}{GUARD}"))?;
    put(
        &hooks.join("reference-transaction"),
        &format!("{HEADER}{REF_GUARD}"),
    )?;
    for verb in ["commit", "push"] {
        let hook = hooks.join(format!("pre-{verb}"));
        let call = format!("\"$(dirname \"$0\")/mux-guard\" {verb} || exit 1  # git-mux\n");
        let Ok(text) = fs::read_to_string(&hook) else {
            continue;
        };
        if !text.contains(&call) {
            let (first, rest) = text.split_once('\n').unwrap_or((&text, ""));
            put(&hook, &format!("{first}\n{call}{rest}"))?;
        }
    }
    let _ = fs::remove_dir_all(repo.state.join("bin"));
    let root = repo.main_root();
    let mut roots = registered_roots();
    if !roots.contains(&root) {
        roots.push(root);
        let file = registry();
        fs::create_dir_all(file.parent().unwrap())?;
        let text: String = roots.iter().map(|r| format!("{}\n", r.display())).collect();
        write_atomic(&file, text.as_bytes(), None)?;
    }
    Ok(())
}

fn is_ours(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| c.contains("git-mux") || c.contains("/mux/bin/hook"))
}

/// Replaces any earlier git-mux entry in one agent's hook config with the current binary.
fn wire(file: &Path, command: &str, pre: &str, post: &str) -> Result<bool> {
    let mut config: Value = match fs::read(file) {
        Ok(data) => serde_json::from_slice(&data)
            .map_err(|e| crate::run::Refused(format!("{}: {e}", file.display())))?,
        Err(_) => json!({}),
    };
    let before = config.clone();
    if !config["hooks"].is_object() {
        config["hooks"] = json!({});
    }
    for (event, matcher) in [("PreToolUse", pre), ("PostToolUse", post)] {
        let mut groups: Vec<Value> = config["hooks"][event]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for g in &mut groups {
            if let Some(hs) = g["hooks"].as_array_mut() {
                hs.retain(|h| !is_ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
        let hook = json!({"type": "command", "command": command, "timeout": 10, "statusMessage": "git-mux"});
        groups.push(json!({"matcher": matcher, "hooks": [hook]}));
        config["hooks"][event] = Value::Array(groups);
    }
    if config == before {
        return Ok(false);
    }
    fs::create_dir_all(file.parent().unwrap())?;
    write_atomic(
        file,
        format!("{}\n", serde_json::to_string_pretty(&config).unwrap()).as_bytes(),
        None,
    )?;
    Ok(true)
}

/// `git mux install`: the git guards, plus the tool hooks of every agent installed for this user.
pub fn install(repo: &Repo) -> Result<()> {
    ensure(repo)?;
    let exe = std::env::current_exe()?;
    let command = format!(
        "'{}' hook",
        exe.display().to_string().replace('\'', r"'\''")
    );
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let agents: [(&str, PathBuf, &str, &str); 2] = [
        (
            "Claude Code",
            home.join(".claude/settings.json"),
            "Bash|Edit|Write|MultiEdit|NotebookEdit",
            "Bash",
        ),
        (
            "Codex",
            home.join(".codex/hooks.json"),
            "^(Bash|apply_patch|Edit|Write)$",
            "^Bash$",
        ),
    ];
    for (name, file, pre, post) in agents {
        if !file.parent().is_some_and(Path::is_dir) {
            continue;
        }
        let changed = wire(&file, &command, pre, post)?;
        println!(
            "git-mux: {name} hooks in {} {}",
            file.display(),
            if changed {
                "updated"
            } else {
                "already current"
            }
        );
        if changed && name == "Codex" {
            println!("git-mux: Codex runs a changed hook only after you trust it once with /hooks");
        }
    }
    println!(
        "git-mux: {} is enrolled; agents commit with `git mux commit -m MSG -- PATH...`",
        repo.main_root().display()
    );
    Ok(())
}
