mod common;

use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn verification_slots_are_locked_against_cleanup() {
    let t = T::new();
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: a2", "--", "a.txt"]);
    let slot = t.tmp.join("slots/mux-verify-0");
    assert!(!t.git_ok(&t.repo, &["worktree", "remove", &slot.to_string_lossy()]));
    assert!(slot.join(".git").exists());
}

/// The pre-commit framework stashes every unstaged file in the live tree when it runs there;
/// through git-mux its hooks run in a slot and the live tree keeps everyone's edits.
#[test]
fn pre_commit_framework_runs_in_the_slot() {
    let present = Command::new("pre-commit")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !present && std::env::var_os("GITMUX_TEST_PRE_COMMIT").is_none() {
        eprintln!("pre-commit is not installed; set GITMUX_TEST_PRE_COMMIT=1 to require it");
        return;
    }
    let t = T::new();
    t.write(
        ".pre-commit-config.yaml",
        "repos:\n  - repo: local\n    hooks:\n      - id: fixme\n        name: rewrite fixme\n        entry: perl -pi -e 's/fixme/fixed/g'\n        language: system\n        files: '\\.txt$'\n",
    );
    t.mux(&[
        "commit",
        "-m",
        "area: hooks",
        "--",
        ".pre-commit-config.yaml",
    ]);
    for hook in ["pre-commit", "commit-msg", "pre-push"] {
        fs::remove_file(t.repo.join(".git/hooks").join(hook)).unwrap();
    }
    t.sh(&t.repo, &["pre-commit", "install"]);
    t.write("b.txt", "another session's unsaved edit\n");
    t.write("a.txt", "fixme\n");
    t.mux(&["commit", "-m", "area: a", "--", "a.txt"]);
    assert_eq!(t.head_file("a.txt"), "fixed\n");
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
    assert_eq!(t.sh(&t.repo, &["git", "stash", "list"]), "");
    assert!(!t.git_ok(&t.repo, &["commit", "-qam", "area: plain"]));
    assert_eq!(t.read("b.txt"), "another session's unsaved edit\n");
}

fn executable(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn refusal(t: &T, args: &[&str]) -> String {
    let o = t.run(args, &[]);
    assert!(!o.status.success(), "{args:?} passed");
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// husky regenerates an ignored hooks directory: the slot's checkout does not have it, so
/// git-mux copies it, and the hooks judge the commit there.
#[test]
fn husky_style_hooks_run_in_the_slot() {
    let t = T::new();
    let dir = t.repo.join(".husky/_");
    executable(
        &dir.join("h"),
        "#!/bin/sh\nfiles=$(git diff --cached --name-only -- '*.txt')\n[ -n \"$files\" ] && grep -l BAD $files && exit 1\nexit 0\n",
    );
    executable(
        &dir.join("pre-commit"),
        "#!/bin/sh\n. \"$(dirname \"$0\")/h\"\n",
    );
    t.write(".husky/_/.gitignore", "*\n");
    t.sh(&t.repo, &["git", "config", "core.hooksPath", ".husky/_"]);
    t.mux(&["install"]);
    let head = t.rev(&t.repo, "HEAD");

    t.write("a.txt", "BAD\n");
    let stderr = refusal(&t, &["commit", "-m", "area: bad", "--", "a.txt"]);
    assert!(stderr.contains("a.txt"), "{stderr}");
    assert_eq!(t.rev(&t.repo, "HEAD"), head);

    t.write("a.txt", "good\n");
    t.mux(&["commit", "-m", "area: good", "--", "a.txt"]);
    assert_eq!(t.head_file("a.txt"), "good\n");
}

/// A hook that exists in the checkout (staged, not committed) but not in the commit's tree
/// would be skipped silently by git; the commit is refused instead.
#[test]
fn a_hook_the_slot_lacks_refuses() {
    let t = T::new();
    executable(&t.repo.join("hooks/pre-commit"), "#!/bin/sh\nexit 0\n");
    t.sh(&t.repo, &["git", "config", "core.hooksPath", "hooks"]);
    t.sh(&t.repo, &["git", "add", "hooks/pre-commit"]);
    let head = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a2\n");
    let stderr = refusal(&t, &["commit", "-m", "area: a2", "--", "a.txt"]);
    assert!(stderr.contains("pre-commit"), "{stderr}");
    assert_eq!(t.rev(&t.repo, "HEAD"), head);
}

/// What `slotSetup` leaves in the slot is there when the hooks run; it sees where it runs and no
/// literal-pathspec setting. A failing setup stops the commit before anything is published.
#[test]
fn slot_setup_runs_before_the_hooks_and_can_refuse() {
    let t = T::new();
    t.write(".git/info/exclude", "setup.out\n");
    executable(
        &t.repo.join(".git/hooks/pre-commit"),
        "#!/bin/sh\ncp setup.out \"$MUX_PROBE\"\n",
    );
    t.sh(
        &t.repo,
        &[
            "git",
            "config",
            "gitmux.slotSetup",
            "printf '%s\\n%s\\n%s\\n' \"$GITMUX_LIVE\" \"$GITMUX_SLOT\" \"${GIT_LITERAL_PATHSPECS-unset}\" > setup.out",
        ],
    );
    let probe = t.tmp.join("probe");
    t.write("a.txt", "a2\n");
    let o = t.run(
        &["commit", "-m", "area: a2", "--", "a.txt"],
        &[("MUX_PROBE", &probe)],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read_to_string(&probe).unwrap(),
        format!(
            "{}\n{}\nunset\n",
            t.repo.display(),
            t.tmp.join("slots/mux-verify-0").display()
        )
    );

    t.sh(
        &t.repo,
        &[
            "git",
            "config",
            "gitmux.slotSetup",
            "echo setup exploded >&2; exit 3",
        ],
    );
    let head = t.rev(&t.repo, "HEAD");
    t.write("a.txt", "a3\n");
    let stderr = refusal(&t, &["commit", "-m", "area: a3", "--", "a.txt"]);
    assert!(stderr.contains("setup exploded"), "{stderr}");
    assert_eq!(t.rev(&t.repo, "HEAD"), head);
}

/// With no `gitmux.slotDir`, slots live in the user's cache, one directory per repository,
/// locked like any slot.
#[test]
fn the_default_slot_dir_is_in_the_cache() {
    let t = T::new();
    t.sh(&t.repo, &["git", "config", "--unset", "gitmux.slotDir"]);
    t.write("a.txt", "a2\n");
    t.mux(&["commit", "-m", "area: a2", "--", "a.txt"]);
    let slots: Vec<_> = fs::read_dir(t.tmp.join("cache/gitmux/slots"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(slots.len(), 1, "{slots:?}");
    let name = slots[0].file_name().unwrap().to_string_lossy().into_owned();
    let (stem, hash) = name.rsplit_once('-').unwrap();
    assert_eq!(stem, "repo");
    assert!(hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()));
    let slot = slots[0].join("mux-verify-0");
    assert!(slot.join(".git").exists());
    assert!(!t.git_ok(&t.repo, &["worktree", "remove", &slot.to_string_lossy()]));
}

/// `selfInstall` replaces the running binary, so a committed `.gitmux` cannot switch it on.
#[test]
fn self_install_is_read_from_local_config_only() {
    let t = T::new();
    fs::create_dir(t.repo.join("tool")).unwrap();
    t.write(".gitmux", "[gitmux]\n\tselfInstall = tool\n");
    t.write("tool/Cargo.toml", "this is not a manifest\n");
    let o = t.mux(&["commit", "-m", "area: tool", "--", "tool/Cargo.toml"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!said.contains("installing"), "{said}");

    t.sh(&t.repo, &["git", "config", "gitmux.selfInstall", "tool"]);
    t.write("tool/Cargo.toml", "still not a manifest\n");
    let o = t.mux(&["commit", "-m", "area: tool", "--", "tool/Cargo.toml"]);
    let said = String::from_utf8_lossy(&o.stderr);
    assert!(said.contains("installing its git-mux failed"), "{said}");
}
