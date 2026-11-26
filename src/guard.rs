use crate::repo::{Repo, unique, write_atomic};
use crate::run::Result;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

const HEADER: &str = "#!/bin/sh\n# Installed by git-mux (https://github.com/nikolay-e/gitmux); rewritten on every git-mux run.\n";
const MARK: &str = "# git-mux";
const CHAINED: &str = "gitmux-chained";
const REF_HOOK: &str = "reference-transaction";
const GUARDED: [(&str, &str); 4] = [
    ("pre-commit", "commit"),
    ("prepare-commit-msg", "commit open"),
    ("commit-msg", "commit"),
    ("pre-push", "push"),
];
const CONFIGURED: [&str; 4] = ["pre-commit", "prepare-commit-msg", "commit-msg", "pre-push"];

/// Written to `<common>/gitmux/guard`, outside the hooks dir; each hook calls it with its verb.
/// No hook runs in the live tree of the shared checkout, so GIT_MUX opens only a guard line marked
/// `open`: prepare-commit-msg's, the one hook the deliberate `GIT_MUX=1 git commit --no-verify` runs.
const GUARD: &str = r#"# Linked worktrees are not guarded: there a commit or push touches no other session's files.
[ "$(git rev-parse --absolute-git-dir)" = "$(git rev-parse --path-format=absolute --git-common-dir)" ] || exit 0
[ "$2" = open ] && [ -n "$GIT_MUX" ] && exit 0
echo "git-mux: plain git $1 in the shared checkout $(git rev-parse --show-toplevel) is refused:" \
  "its hooks would run among other sessions' unsaved files. Commit with: git mux commit -m MSG -- PATH...;" \
  "push with: git mux push, or push from a linked worktree or a bare clone" >&2
exit 1
"#;

/// The shared branch moves only through git-mux, from any worktree, except a fast-forward to
/// commits a remote already has (a plain pull sweeps in nothing local); gc re-packs a ref at its
/// own value. A new stash is refused in the shared checkout only. HEAD is left to the tool hook:
/// a branch switch and a new worktree's HEAD look alike here, and a switch rewrites files first.
/// An all-zero id is a deletion in any hash length: only the shared branch's is refused, except
/// the one git's own pack-refs makes, which prunes the loose file of a ref already packed at the
/// same value (a deletion by hand reports an all-zero old id first, or a loose-only ref).
const REF_GATE: &str = "[ \"$1\" = prepared ] && [ -z \"$GIT_MUX\" ] || exit 0\n";

const REF_CHECK: &str = r#"common=$(git rev-parse --path-format=absolute --git-common-dir)
branch=$(git --git-dir="$common" symbolic-ref -q HEAD)
while read -r old new ref; do
  case "$ref" in
    "$branch") ;;
    refs/stash) [ "$(git rev-parse --absolute-git-dir)" = "$common" ] || continue ;;
    *) continue ;;
  esac
  case "$new" in
    *[!0]*) ;;
    *)
      [ "$ref" = "$branch" ] || continue
      [ -f "$common/$ref" ] && grep -Fxq "$old $ref" "$common/packed-refs" 2>/dev/null && continue
      ;;
  esac
  [ "$(git rev-parse -q --verify "$ref")" = "$new" ] && continue
  case "$old" in *[!0]*) existed=1 ;; *) existed= ;; esac
  if [ "$ref" = "$branch" ] && [ -n "$existed" ] && git merge-base --is-ancestor "$old" "$new" 2>/dev/null &&
    [ -n "$(git for-each-ref --count=1 --contains "$new" refs/remotes)" ]; then
    continue
  fi
  echo "git-mux: refused to move $ref outside git-mux; it holds every session's work in $(dirname "$common")." \
    "Commit with: git mux commit -m MSG -- PATH...; or set GIT_MUX=1 for one deliberate command." >&2
  exit 1
done
"#;

/// Another tool's reference-transaction hook keeps running: its stdin is saved, the check runs in
/// the "prepared" state, and the saved stdin feeds the chained hook in every state.
fn ref_guard(chained: bool) -> String {
    if !chained {
        return format!("{HEADER}{REF_GATE}{REF_CHECK}");
    }
    format!(
        r#"{HEADER}tmp=$(mktemp "${{TMPDIR:-/tmp}}/gitmux-ref.XXXXXX") || exit 1
cat > "$tmp"
if [ "$1" = prepared ] && [ -z "$GIT_MUX" ]; then
(
{REF_CHECK}) < "$tmp" || {{ rm -f "$tmp"; exit 1; }}
fi
chained="$(dirname "$0")/{CHAINED}/{REF_HOOK}"
[ -x "$chained" ] || {{ rm -f "$tmp"; exit 0; }}
"$chained" "$@" < "$tmp"
status=$?
rm -f "$tmp"
exit $status
"#
    )
}

fn guard_line(verb: &str) -> String {
    format!(
        "g=\"$(git rev-parse --path-format=absolute --git-common-dir)/gitmux/guard\"; [ ! -x \"$g\" ] || \"$g\" {verb} || exit 1  {MARK}\n"
    )
}

fn marked(line: &str) -> bool {
    line.trim_end().ends_with(MARK)
}

fn without_marks(text: &str) -> String {
    text.split_inclusive('\n').filter(|l| !marked(l)).collect()
}

/// The text with the one current guard line after the shebang (first, when there is none).
fn with_line(text: &str, line: &str) -> String {
    let clean = without_marks(text);
    if clean.starts_with("#!") {
        let (first, rest) = clean.split_once('\n').unwrap_or((&clean, ""));
        format!("{first}\n{line}{rest}")
    } else {
        format!("{line}{clean}")
    }
}

fn is_shell(text: &str) -> bool {
    let Some(shebang) = text.lines().next().and_then(|l| l.strip_prefix("#!")) else {
        return true;
    };
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_string();
    let mut words = shebang.split_whitespace();
    let mut program = words.next().map(base).unwrap_or_default();
    if program == "env" {
        program = words
            .find(|w| !w.starts_with('-') && !w.contains('='))
            .map(base)
            .unwrap_or_default();
    }
    matches!(
        program.as_str(),
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "ash"
    )
}

fn put(path: &Path, text: &str) -> Result<()> {
    let same = fs::read(path).is_ok_and(|d| d == text.as_bytes())
        && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if !same {
        write_atomic(path, text.as_bytes(), Some(0o755))?;
    }
    Ok(())
}

fn runs(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink())
}

fn tracked(repo: &Repo, path: &Path) -> bool {
    repo.git()
        .args(["ls-files", "-z", "--"])
        .arg(path)
        .out()
        .is_ok_and(|o| o.ok() && !o.stdout.is_empty())
}

fn hooks_dir(repo: &Repo) -> Result<PathBuf> {
    Ok(PathBuf::from(
        repo.git()
            .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
            .text()?,
    ))
}

/// A relative link keeps pointing at the same file from one directory deeper.
fn deeper(link: &Path) -> PathBuf {
    if link.is_absolute() {
        link.to_path_buf()
    } else {
        PathBuf::from(format!("../{}", link.display()))
    }
}

fn shallower(link: &Path) -> PathBuf {
    let text = link.to_string_lossy();
    PathBuf::from(text.strip_prefix("../").unwrap_or(&text))
}

/// Copies a hook git-mux does not own into `<hooks>/gitmux-chained/` and leaves the original in
/// place, so the hook never goes missing; the caller then replaces the original.
fn chain(hooks: &Path, name: &str) -> Result<()> {
    let dir = hooks.join(CHAINED);
    fs::create_dir_all(&dir)?;
    let (hook, copy) = (hooks.join(name), dir.join(name));
    let _ = fs::remove_file(&copy);
    match fs::read_link(&hook) {
        Ok(link) => symlink(deeper(&link), &copy)?,
        Err(_) => {
            fs::copy(&hook, &copy)?;
        }
    }
    Ok(())
}

fn place(
    hooks: &Path,
    name: &str,
    verb: &str,
    tracked_dir: bool,
    notes: &mut Vec<String>,
) -> Result<()> {
    let hook = hooks.join(name);
    if !runs(&hook) {
        return Ok(());
    }
    let line = guard_line(verb);
    let text = fs::read_to_string(&hook).ok();
    if tracked_dir {
        if !text.is_some_and(|t| t.contains(&line)) {
            notes.push(format!(
                "git-mux: {} is tracked by git and left alone; to guard it add this line after its shebang:\n{}",
                hook.display(),
                line.trim_end()
            ));
        }
        return Ok(());
    }
    let wrapper = format!("{HEADER}{line}exec \"$(dirname \"$0\")/{CHAINED}/{name}\" \"$@\"\n");
    match text {
        Some(t) if !is_link(&hook) && t.starts_with(HEADER) => {
            if exists(&hooks.join(CHAINED).join(name)) {
                put(&hook, &wrapper)
            } else {
                put(&hook, &format!("{HEADER}{line}"))
            }
        }
        Some(t) if !is_link(&hook) && is_shell(&t) => {
            let new = with_line(&t, &line);
            if new != t {
                let mode = fs::metadata(&hook)?.permissions().mode() & 0o7777;
                write_atomic(&hook, new.as_bytes(), Some(mode))?;
            }
            Ok(())
        }
        _ => {
            chain(hooks, name)?;
            put(&hook, &wrapper)
        }
    }
}

fn place_ref_hook(hooks: &Path) -> Result<()> {
    let hook = hooks.join(REF_HOOK);
    let ours = !is_link(&hook) && fs::read_to_string(&hook).is_ok_and(|t| t.starts_with(HEADER));
    if exists(&hook) && !ours {
        chain(hooks, REF_HOOK)?;
    }
    let chained = exists(&hooks.join(CHAINED).join(REF_HOOK));
    put(&hook, &ref_guard(chained))
}

/// Hooks that configuration (git >= 2.54) runs before the hook file, so no hook file can stop them.
fn configured_notes(repo: &Repo) -> Vec<String> {
    let Ok(out) = repo
        .git()
        .args(["config", "--get-regexp", r"^hook\..*\.event$"])
        .out()
    else {
        return Vec::new();
    };
    out.text()
        .lines()
        .filter_map(|l| {
            let (key, event) = l.split_once(' ')?;
            let name = key.strip_prefix("hook.")?.strip_suffix(".event")?;
            CONFIGURED.contains(&event).then(|| {
                format!(
                    "git-mux: the configured hook {name} runs on {event} before the guard, so a person's plain git commit or push still runs it in the live tree; agents are refused by the agent hook"
                )
            })
        })
        .collect()
}

/// Git itself refuses the other ways in, for any process: a reference-transaction hook and a
/// guard in front of the pre-commit and pre-push hooks. Returns the notes to print.
pub fn ensure(repo: &Repo) -> Result<Vec<String>> {
    let hooks = hooks_dir(repo)?;
    let tracked_dir = tracked(repo, &hooks);
    let mut notes = configured_notes(repo);
    put(&repo.state.join("guard"), &format!("{HEADER}{GUARD}"))?;
    if !tracked_dir {
        let _ = fs::remove_file(hooks.join("mux-guard"));
    }
    if !tracked_dir {
        fs::create_dir_all(&hooks)?;
        let pre_commit = hooks.join("pre-commit");
        if !exists(&pre_commit) {
            put(&pre_commit, &format!("{HEADER}{}", guard_line("commit")))?;
        }
    }
    for (name, verb) in GUARDED {
        place(&hooks, name, verb, tracked_dir, &mut notes)?;
    }
    if tracked_dir {
        notes.push(format!(
            "git-mux: {} is tracked by git, so the reference-transaction guard is not installed there",
            hooks.display()
        ));
    } else {
        place_ref_hook(&hooks)?;
    }
    Ok(notes)
}

fn restore(hooks: &Path, name: &str) -> Result<()> {
    let (hook, copy) = (hooks.join(name), hooks.join(CHAINED).join(name));
    if !exists(&copy) {
        let _ = fs::remove_file(&hook);
        return Ok(());
    }
    if let Ok(link) = fs::read_link(&copy) {
        let tmp = unique(hooks, ".gitmux-restore-");
        symlink(shallower(&link), &tmp)?;
        fs::rename(&tmp, &hook)?;
        fs::remove_file(&copy)?;
    } else {
        fs::rename(&copy, &hook)?;
    }
    Ok(())
}

/// Takes git-mux out of the hooks dir: marker lines dropped, chained hooks restored byte for
/// byte, wrappers, the reference-transaction guard and the guard script deleted.
pub fn remove(repo: &Repo) -> Result<()> {
    let hooks = hooks_dir(repo)?;
    if !tracked(repo, &hooks) {
        let _ = fs::remove_file(hooks.join("mux-guard"));
        for name in GUARDED.map(|(name, _)| name).into_iter().chain([REF_HOOK]) {
            let hook = hooks.join(name);
            let Some(text) = fs::read_to_string(&hook).ok().filter(|_| !is_link(&hook)) else {
                continue;
            };
            if text.starts_with(HEADER) {
                restore(&hooks, name)?;
            } else if text.lines().any(marked) {
                let mode = fs::metadata(&hook)?.permissions().mode() & 0o7777;
                write_atomic(&hook, without_marks(&text).as_bytes(), Some(mode))?;
            }
        }
        let _ = fs::remove_dir(hooks.join(CHAINED));
    }
    let _ = fs::remove_file(repo.state.join("guard"));
    Ok(())
}
