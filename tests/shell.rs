mod common;

use common::*;
use serde_json::json;

/// `sh` fed a here-document that holds another `sh`, `levels` deep, around `inner`.
fn nested(levels: usize, inner: &str) -> String {
    (1..=levels).rev().fold(inner.to_string(), |body, i| {
        format!("sh <<D{i}\n{body}\nD{i}")
    })
}

#[test]
fn shell_guard() {
    let t = T::new();
    t.sh(&t.repo, &["git", "config", "alias.wipe", "clean -fd"]);
    t.sh(&t.repo, &["git", "config", "alias.bang", "!git clean -fd"]);
    let repo = t.repo.to_string_lossy().into_owned();
    let refused = |command: &str| {
        t.hook(
            "A",
            "Bash",
            json!({"command": command.replace("{repo}", &repo)}),
        )
        .is_some()
    };
    let argv_refused =
        |argv: serde_json::Value| t.hook("A", "Bash", json!({"command": argv})).is_some();

    let cases = [
        // where a git word may stand
        ("git status\ngit clean -fd", true),
        ("A=1 git rebase main", true),
        ("if true; then git clean -fd; fi", true),
        ("while true; do git clean -fd; done", true),
        ("git status && git clean -fd", true),
        ("git status || git clean -fd", true),
        ("! git clean -fd", true),
        ("{ git clean -fd; }", true),
        ("( git clean -fd )", true),
        ("case x in a) git clean -fd;; esac", true),
        ("env GIT_TERMINAL_PROMPT=0 git clean -fd", true),
        ("sudo git clean -fd", true),
        ("nohup git clean -fd", true),
        ("command git clean -fd", true),
        ("exec git clean -fd", true),
        ("nice -n 5 git clean -fd", true),
        ("time git clean -fd", true),
        ("timeout 5 git push", true),
        ("xargs git clean -fd", true),
        (r"find . -exec git checkout . \;", true),
        (r"find . -exec git status \; -exec git clean -fd \;", true),
        ("/usr/bin/git clean -fd", true),
        ("'git' clean -fd", true),
        ("\"git\" clean -fd", true),
        ("\\git clean -fd", true),
        // substitutions, wrapped shells, eval, here-documents
        ("echo \"$(git reset --hard)\"", true),
        ("x=`git stash`", true),
        ("echo \"`git stash`\"", true),
        ("echo $(echo $(git clean -fd))", true),
        ("cat <(git clean -fd)", true),
        ("tee >(git clean -fd)", true),
        ("bash -lc 'git clean -fd'", true),
        ("bash -ec 'git clean -fd'", true),
        ("bash -x -c 'git clean -fd'", true),
        ("zsh -c 'git clean -fd'", true),
        ("sudo -u x /bin/sh -c \"git clean -fd\"", true),
        ("sh -c 'bash -c \"git clean -fd\"'", true),
        ("eval git stash", true),
        ("eval 'git' 'stash'", true),
        ("bash <<'EOF'\ngit clean -fd\nEOF", true),
        ("bash <<EOF\ngit clean -fd\nEOF", true),
        ("sh -s <<'EOF'\ngit clean -fd\nEOF", true),
        ("bash <<< 'git clean -fd'", true),
        ("cat <<EOF\n$(git clean -fd)\nEOF", true),
        (&nested(8, "git clean -fd"), true),
        // data, not commands
        ("echo 'git clean -fd'", false),
        ("echo \"git clean -fd\"", false),
        ("cat <<'EOF' > note\ngit clean -fd\nEOF\ngit status", false),
        ("cat <<'EOF'\n$(git clean -fd)\nEOF", false),
        ("git log --grep git", false),
        ("git mux commit -m 'git commit' -- a.txt", false),
        (&nested(8, "git status"), false),
        // unparseable text in the checkout is refused
        ("echo \"git clean -fd", true),
        (&nested(9, "git status"), true),
        // directories
        ("git -C /tmp clean -fd", false),
        ("git -C /tmp -C {repo} clean -fd", true),
        ("cd /tmp && git clean -fd", false),
        ("cd /tmp; cd {repo}; git clean -fd", true),
        ("cd /tmp && git -C {repo} clean -fd", true),
        ("(cd /tmp); git clean -fd", true),
        ("(cd /tmp && git status); git clean -fd", true),
        ("cd /tmp && (cd {repo} && git clean -fd)", true),
        ("cd /tmp | git clean -fd", true),
        ("cd /tmp & git clean -fd", true),
        ("cd -P /tmp && git clean -fd", false),
        ("cd ~ && git clean -fd", false),
        ("cd $HOME/../repo && git clean -fd", true),
        ("cd ${HOME}/../repo && git clean -fd", true),
        ("pushd /tmp && git clean -fd", false),
        ("cd \"$(pwd)\" && git clean -fd", true),
        ("cd $X && git clean -fd", true),
        ("cd - && git clean -fd", true),
        ("git log -C", false),
        ("git --git-dir={repo}/.git clean -fd", true),
        (
            "git --git-dir {repo}/.git --work-tree={repo} clean -fd",
            true,
        ),
        ("GIT_DIR={repo}/.git git clean -fd", true),
        (
            "git --git-dir=/tmp/x/.git --work-tree=/tmp/x clean -fd",
            false,
        ),
        (
            "GIT_WORK_TREE=/tmp/x GIT_DIR=/tmp/x/.git git clean -fd",
            false,
        ),
        // what git itself would run
        ("git x", false),
        ("git wipe", true),
        ("git bang", true),
        ("git -c alias.x='!git clean -fd' x", true),
        (
            "git -c core.hooksPath=/dev/null update-ref refs/heads/x HEAD",
            true,
        ),
        ("git update-ref refs/heads/x HEAD", false),
        ("git config core.hooksPath /tmp", true),
        ("git config --unset core.hooksPath", true),
        (
            "git config --global --replace-all core.hooksPath /tmp",
            true,
        ),
        ("git config core.hooksPath", false),
        ("git config --get core.hooksPath", false),
        ("git config user.name x", false),
        // refusals
        ("git add -A", true),
        ("git add a.txt", false),
        ("git checkout -- a.txt", false),
        ("git checkout feature", true),
        ("git reset --hard", true),
        ("git reset -- a.txt", false),
        ("git pull --rebase", true),
        ("git commit --no-verify -m x", true),
        ("FOO=1 git commit -m x", true),
        ("git -C /tmp commit -m x", false),
        ("git stash", true),
        ("git stash push -m x", true),
        ("git stash pop", true),
        ("git stash list", false),
        ("git stash show -p", false),
        ("git stash create", false),
        ("git push", true),
        ("git push --no-verify", true),
        ("git push origin v1", true),
        ("git mux push", false),
        ("git bisect start", true),
        ("git filter-branch -f", true),
        ("git sparse-checkout set a", true),
        ("git sparse-checkout disable", true),
        ("git sparse-checkout list", false),
        ("git read-tree -u HEAD", true),
        ("git read-tree -m HEAD", true),
        ("git read-tree --reset HEAD", true),
        ("git read-tree HEAD", false),
        ("git checkout-index -a", true),
        ("git checkout-index -f a.txt", true),
        ("git checkout-index a.txt", false),
        ("git rm -r .", true),
        ("git rm -r -- :/", true),
        ("git rm 'a*'", true),
        ("git rm -r {repo}/.git/hooks", true),
        ("git rm a.txt", false),
        ("git mux commit -m 'area: x' -- a.txt", false),
    ];
    for (command, blocked) in cases {
        assert_eq!(refused(command), blocked, "{command}");
    }

    assert!(argv_refused(json!(["git", "clean", "-fd"])));
    assert!(argv_refused(json!(["bash", "-lc", "git clean -fd"])));
    assert!(argv_refused(json!(["sudo", "git", "push"])));
    assert!(argv_refused(json!([
        "git", "-C", &repo, "commit", "-m", "x"
    ])));
    assert!(!argv_refused(json!(["git", "status"])));
    assert!(!argv_refused(json!(["git", "-C", "/tmp", "clean", "-fd"])));
    assert!(!argv_refused(json!(["git", "log", "-C"])));

    let elsewhere = json!({"session_id": "A", "cwd": "/", "tool_name": "Bash", "hook_event_name": "PreToolUse",
                           "tool_input": {"command": format!("cd {repo} && git clean -fd")}});
    assert_eq!(t.hook_raw(&elsewhere.to_string()).status.code(), Some(2));
}
