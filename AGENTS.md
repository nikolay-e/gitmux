# gitmux

## Ultimate Goal

Let any number of agents and people work in one checkout at once without ever losing a line of
each other's work.

## Invariants

- Nothing git-mux does writes the shared working tree or the shared index except the paths a
  commit names, a hook fix to a file still equal to what was verified, and a sync's upstream
  changes to files nobody has edited or staged. Every other step runs in the object store, a
  private index or a verification slot.
- The branch moves only by compare-and-swap (`git update-ref <ref> <new> <old>`). A lost race is
  merged in the object store and re-verified only when the other commit reached a component this
  one depends on.
- Verification runs the repository's own hooks through `git hook run`, with the slot's index at
  the candidate and `HEAD` at its parent: what a plain `git commit` would run, for any hook
  manager. git-mux never names a linter or a framework.
- Slots are persistent and `git worktree lock`ed: they hold built dependencies, and cleanup must
  not delete them.
- The agent hook fails closed: a crash, an unreadable payload or a git error exits 2, which both
  Claude Code and Codex honour as a refusal. A call that concerns no enrolled checkout exits 0
  before parsing anything.
- A lease belongs to the agent process (pid plus start time), not to the session id, so a gone
  agent frees its files. A shell command's writes become its session's only when no other
  session was active in the same window; otherwise they stay unowned.
- Publishing writes a journal first; every run heals an interrupted one before doing anything.

## Traps

- The binary is `git-mux`, so `git mux …` works; the shell parser treats `git mux` and
  `…/git-mux` alike, and the guard must never refuse it.
- `reference-transaction` sees a branch switch and a new worktree's `HEAD` alike, so `HEAD` moves
  are left to the agent hook; the git hook guards the branch ref and `refs/stash`.
- Empty path lists mean "nothing", never "the whole tree": `git ls-files` and `checkout` with no
  pathspec act on everything.
- Tests drive the real binary against real repositories, hooks and processes
  (`tests/shared_checkout.rs`); they set `HOME` and `XDG_CONFIG_HOME` per test so nothing reaches
  the developer's agent config.
- This crate lives in a monorepo and is published by `git subtree split --prefix=gitmux`: it must
  not name the monorepo, its hosts or its people.
