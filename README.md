# git-mux

Several coding agents (Claude Code, Codex, …) editing **one checkout, on one branch, at the same
time** — and committing without wiping out each other's work.

Plain `git commit` breaks down there:

- the pre-commit framework stashes every unstaged file while its hooks run, so another agent's
  half-written file disappears mid-edit;
- the shared index sweeps in whatever another agent staged;
- whole-project hooks judge a mix of everyone's unfinished files;
- `git stash`, `git reset --hard`, `git checkout <branch>` or `git clean` from one agent destroy
  the others' work.

git-mux is one static binary that replaces those paths.

## What it does

- **`git mux commit -m MSG -- PATH...`** commits only the named paths. It builds the candidate
  in a private index from `HEAD`, runs the repository's own `pre-commit` and `commit-msg` hooks
  on exactly that candidate in a persistent verification worktree, writes formatter fixes back
  only to files nobody touched meanwhile, and moves the branch with a compare-and-swap. If
  another agent landed first, the candidate is merged onto the new tip inside the object store
  (`git merge-tree`); the working tree and the shared index are never used.
- **`git mux push`** replays local commits onto the upstream (sparing every file in use), runs
  the `pre-push` hook on that exact commit in a verification worktree, and pushes it.
- **`git mux sync`** does the replay alone. **`git mux status`** shows which agent holds which
  uncommitted file.
- **Git itself refuses** a plain `commit`, `stash` or `push` in the shared checkout, and a
  `reference-transaction` hook refuses any other move of the shared branch except a
  fast-forward to commits a remote already has. Linked worktrees stay free. `GIT_MUX=1`
  overrides one deliberate command.
- **Agent hooks** (`git mux hook`, wired by `git mux install`) refuse destructive git commands
  in the shared checkout before they run, give each file an owner (a *lease*) when an agent
  edits it, refuse another agent's edit or commit of a leased file, and attribute files a shell
  command wrote. A lease ends when the file matches `HEAD` again or its agent process exits.
  The hook fails closed: a crash or an unreadable payload refuses the call.
- An interrupted commit or sync is finished by the next run.

Any hook manager works — pre-commit, husky, lefthook, plain scripts — because git-mux runs
`git hook run` with the index at the candidate and `HEAD` at its parent, which is what a plain
`git commit` would show the hooks.

## Install

Requires git 2.47 or newer and a Rust toolchain ≥ 1.89, on macOS or Linux (both tested).

```sh
cargo install --locked --git https://github.com/nikolay-e/gitmux
cd /path/to/shared/checkout
git mux install
```

`git mux install` installs the git guards in that repository, enrolls it, and wires the hooks of
every agent it finds for your user (`~/.claude/settings.json`, `~/.codex/hooks.json`), keeping
your other hooks. Codex runs a changed hook only after you trust it once with `/hooks`. Any
`git mux` command in a checkout installs the guards too, so a clone is protected from its first
use.

Then tell your agents to commit with `git mux commit -m MSG -- PATH...` and push with
`git mux push`, for example in the repository's `AGENTS.md`.

## Configuration

`git config` keys, read from the repository's config first and then from a committed `.gitmux`
file (same syntax):

| key | default | meaning |
| --- | --- | --- |
| `gitmux.slots` | `2` | verification worktrees, i.e. commits verified at once |
| `gitmux.slotDir` | `<git-common-dir>/gitmux/slots` | where they live (`~` expands) |
| `gitmux.agent` | `claude`, `codex` | process names whose lifetime bounds a lease (multi-valued) |
| `gitmux.components` | `components.json` | optional monorepo map; a lost race re-verifies only when the other commit touched a component this one depends on |
| `gitmux.selfInstall` | unset | path of this crate inside the repository: a landed commit that touches it rebuilds and replaces the running binary |

State lives in `<git-common-dir>/gitmux/`; the enrolled checkouts are listed in
`$XDG_CONFIG_HOME/gitmux/repos`.

## Develop

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

The tests drive the real binary against real repositories, hooks and concurrent processes.
Set `GITMUX_TEST_PRE_COMMIT=1` to require the pre-commit framework test instead of skipping it
when `pre-commit` is not installed.

## License

MIT
