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
  in a private index from `HEAD`, runs the repository's own `pre-commit`, `prepare-commit-msg`
  and `commit-msg` hooks on exactly that candidate in a persistent verification worktree (a
  *slot*), writes formatter fixes back only to files nobody touched meanwhile, and moves the
  branch with a compare-and-swap. If another agent landed first, the candidate is merged onto the
  new tip inside the object store (`git merge-tree`); the working tree and the shared index are
  never used.
- **`git mux push [REF...]`** replays local commits onto the upstream, runs the `pre-push` hook
  on that exact commit in a slot, and pushes it. With REFs (a branch, a tag or `src:dst`) it
  checks and pushes each one the same way; forced and deleting refspecs are refused.
- **`git mux sync`** does the replay alone. It refuses, and leaves the checkout as it is, when an
  incoming file is edited or staged there. **`git mux status`** shows which agent holds which
  uncommitted file.
- **Agent hooks** (`git mux hook`, wired by `git mux install`) refuse destructive git commands
  in the shared checkout before they run, give each file an owner (a *lease*) when an agent
  edits it, refuse another agent's edit or commit of a leased file, and attribute files a shell
  command wrote.
- An interrupted commit or sync is finished by the next run.

Any hook manager works — pre-commit, husky, lefthook, plain scripts — because git-mux runs
`git hook run` with the index at the candidate and `HEAD` at its parent, which is what a plain
`git commit` would show the hooks. git-mux itself runs only the hooks named above: `post-commit`
is not run, because it would have to run in the live tree to mean anything.

## Install

Needs git 2.40 or newer (`merge-tree --write-tree --merge-base`, `hook run --to-stdin`), 2.41 in a
sparse checkout (`sparse-checkout check-rules -z`). Developed on macOS with git 2.56.1; CI uses the
git its GitHub runners ship.

From a release (static Linux builds for x86_64 and aarch64, macOS for x86_64 and arm64), built by
the public repository's CI. The archive's checksum is listed in `SHA256SUMS`, and its build
provenance can be checked with the GitHub CLI:

```sh
t=aarch64-apple-darwin   # x86_64-apple-darwin, x86_64-unknown-linux-musl, aarch64-unknown-linux-musl
base=https://github.com/nikolay-e/gitmux/releases/latest/download
curl -fsSLO "$base/git-mux-$t.tar.gz" -O "$base/SHA256SUMS"
grep " git-mux-$t.tar.gz\$" SHA256SUMS | shasum -a 256 -c
gh attestation verify "git-mux-$t.tar.gz" --repo nikolay-e/gitmux
tar -xzf "git-mux-$t.tar.gz"   # put git-mux-$t/git-mux on your PATH
```

Or from source, with a Rust toolchain ≥ 1.89:

```sh
cargo install --locked --git https://github.com/nikolay-e/gitmux
```

Then, in the shared checkout:

```sh
git mux install            # enroll this checkout, install its git guards, wire the agents
git mux install --project  # also this checkout's own fail-closed Claude Code hooks
git mux uninstall          # take it all out again
git mux --version
```

`git mux install` enrolls the repository, installs the git guards, and wires the user-level hooks
of every agent whose config directory exists (`~/.claude/settings.json`, `~/.codex/hooks.json`),
keeping your other hooks. Codex runs a changed hook only after you trust it once with `/hooks`.
Only `install` enrolls a checkout; any other `git mux` command merely repairs the guards of one
that is already enrolled.

`--project` writes `.claude/settings.local.json` in the checkout (git is told to ignore it: it
holds the binary's absolute path), so Claude Code refuses a tool call there whenever the hook
cannot run. `uninstall` restores the hook files byte for byte (a `pre-commit` it created is
deleted), removes the slots, the state, the project wiring and the registry entry, and takes the
agents' wiring out with the last checkout.

Then tell your agents to commit with `git mux commit -m MSG -- PATH...` and push with
`git mux push`, for example in the repository's `AGENTS.md`.

## What is refused, and where

Two layers, independent of each other, so that a gap in one does not open the other.

**Git itself**, for any process, in the shared checkout (linked worktrees stay free):

- A `reference-transaction` hook refuses every move of the shared branch except `git mux` and a
  fast-forward to commits a remote already has (a plain `git pull --ff-only` still works), and a
  new `refs/stash`. Deleting the branch (`git update-ref -d`) is refused like moving it. That is
  what stops a plain `git commit`, `git reset --soft` or `git branch -f`, from any worktree.
  `GIT_MUX=1` lets one deliberate ref move through.
- A guard line in front of the repository's `pre-commit`, `prepare-commit-msg`, `commit-msg` and
  `pre-push` hook files refuses a plain `git commit` or `git push` before any of them does its
  work, so no hook file runs in the live tree. A missing `pre-commit` is created for the guard
  (`uninstall` removes it again); the other three are guarded where they exist. A hook that is
  not a shell script or a symlink is chained behind a small wrapper, and a hook manager that
  regenerates its directory (husky) gets the line again on the next `git mux` run. `GIT_MUX`
  never opens the guards of `pre-commit`, `commit-msg` and `pre-push`, so `GIT_MUX=1 git commit`
  is refused. `GIT_MUX=1 git commit --no-verify` is the one deliberate bypass: only
  `prepare-commit-msg`'s line lets `GIT_MUX` through, so that this command runs it, and a plain
  `git commit --no-verify` is refused there.
- A person's `git push --no-verify` is not stopped: git skips the hook that would refuse it. The
  agent hook refuses it.
- A hooks directory that git tracks is never edited. `git mux install` prints the lines to add
  instead, and the `reference-transaction` guard is not installed there.

**The agent hook**, for Claude Code and Codex, in every enrolled checkout:

- `git commit`, `git push`, `git stash`, `git reset`, `git clean`, `git rebase`, `git merge`,
  branch switches, broad `add`/`checkout`/`restore`/`rm` and the like are refused before they
  run, with the reason and the command to use instead. Any word `git` (or `…/git`) starts a git
  invocation wherever it stands: in a subshell, `$(…)`, backticks, `sh -c`, `eval`, behind a
  wrapper, with `-c alias.*` or `-c core.hooksPath`. The price is that an unquoted
  `echo git push` is refused too. In an enrolled checkout, a command that names git and cannot
  be parsed is refused.
- A *lease* belongs to a session, a subagent of it and the agent process hosting both. Another
  session's edit or `git mux commit` of a leased file is refused; two subagents of one session
  hold leases against each other. A lease ends when its file equals `HEAD` again, when its agent
  process or session ends, and a finished subagent hands its files to its session. `git mux
  commit --take` takes files over when their holder agreed.
- A crash, a git error or an unreadable payload that names an enrolled checkout refuses the call
  (exit 2); lifecycle events such as `Stop` never refuse. The hook installed for your user exits
  0 when the binary is missing, so other projects are never blocked; the project hook of
  `--project` (`onFailure: "block"`, Claude Code 2.1.295 or newer) blocks the call in that
  checkout when the hook cannot run. Codex's handling of a hook that cannot run is undocumented:
  treat Codex as fail-open.

**Known limits**: a *configured* hook (`hook.<name>.event`, git 2.54 and newer) is the one hook
that can run in the live tree: it runs before any hook file and cannot be stopped by a guard, so a
person's plain `git commit` still runs it (`git mux status` and `install` name such hooks; agents
are refused before git runs); inline
interpreter code (`python -c`, `node -e`) and script files are not parsed; and a shell command's
writes are attributed to its session only when no other session was active meanwhile.

## Configuration

`git config` keys, read from the repository's config first and then from a committed `.gitmux`
file (same syntax):

| key | default | meaning |
| --- | --- | --- |
| `gitmux.slots` | `2` | verification worktrees, i.e. commits verified at once |
| `gitmux.slotDir` | `${XDG_CACHE_HOME:-~/.cache}/gitmux/slots/<name>-<8 hex>` | where they live (`~` expands; relative to the checkout) |
| `gitmux.slotSetup` | unset | shell command run in each slot before the hooks, with `GITMUX_LIVE` (the checkout) and `GITMUX_SLOT` set; a failure refuses the commit or push |
| `gitmux.agent` | `claude`, `codex` | process names whose lifetime bounds a lease (multi-valued) |
| `gitmux.components` | `components.json` | optional monorepo map; a lost race re-verifies only when the other commit touched a component this one depends on |
| `gitmux.selfInstall` | unset | path of this crate inside the repository: a commit landed by `git mux commit` that touches it rebuilds and replaces the running binary |

`slotSetup` is where a slot gets what its hooks need (built dependencies, a cloned
`node_modules`); git-mux names no framework. A committed `.gitmux` runs its `slotSetup` for
everyone who commits in the checkout, which is the same trust as the repository's own hooks.
`selfInstall` replaces the binary, so only the repository's local git config can set it, never a
committed file, the global config or the system config.

A hooks directory that git ignores and the slot therefore lacks (husky's) is copied into the
slot when `core.hooksPath` is a relative path inside the repository; a hook the checkout has and
the slot lacks refuses the commit.

State lives in `<git-common-dir>/gitmux/`; the enrolled checkouts are listed in
`$XDG_CONFIG_HOME/gitmux/repos` (`~/.config` when unset).

## Develop

```sh
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
```

The tests drive the real binary against real repositories, hooks and concurrent processes.
Set `GITMUX_TEST_PRE_COMMIT=1` to require the pre-commit framework test instead of skipping it
when `pre-commit` is not installed.

## License

MIT
