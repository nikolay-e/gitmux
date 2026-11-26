use crate::repo::{Repo, write_atomic};
use crate::run::{Cmd, Result};
use std::fs;
use std::path::{Path, PathBuf};

const HEADER: &str = "#!/bin/sh\n# Installed by git-mux (https://github.com/nikolay-e/gitmux); rewritten on every git-mux run.\n";

const GUARD: &str = r#"# In the shared checkout a commit, stash or push made outside git-mux stashes or sweeps in other
# sessions' work, so git refuses it here. Linked worktrees are not guarded. GIT_MUX=1 overrides a
# commit; never a push, whose hooks would run in the live tree (git mux push needs none here).
[ -n "$GIT_MUX" ] && [ "$1" != push ] && exit 0
[ "$(git rev-parse --absolute-git-dir)" = "$(git rev-parse --path-format=absolute --git-common-dir)" ] || exit 0
echo "git-mux: plain git $1 in the shared checkout $(git rev-parse --show-toplevel) is refused:" \
  "its hooks would run among other sessions' unsaved files. Commit with: git mux commit -m MSG -- PATH...;" \
  "push with: git mux push, or push from a linked worktree or a bare clone" >&2
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
/// guard in front of the pre-commit and pre-push hooks. Returns the notes to print.
pub fn ensure(repo: &Repo) -> Result<Vec<String>> {
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
    Ok(Vec::new())
}

#[allow(dead_code, unused_variables)]
pub fn remove(repo: &Repo) -> Result<()> {
    Ok(())
}
