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
- No commit or push hook file runs in the live tree of the shared checkout: a guard line in front
  of the pre-commit, prepare-commit-msg, commit-msg and pre-push files refuses a plain commit or
  push before they run (a missing pre-commit is created for it, and uninstall deletes it again).
  Hooks of other commands (pre-merge-commit, pre-rebase, applypatch-msg, post-checkout,
  post-merge, post-rewrite, …) still run there when a person runs those commands; agents are
  refused such commands before git runs, and the ref guard refuses any move of the shared branch.
  `GIT_MUX=1` skips the reference-transaction guard (a deliberate ref move) and
  prepare-commit-msg's line, and nothing else: the one deliberate bypass is `GIT_MUX=1 git commit
  --no-verify`, which runs prepare-commit-msg, while a plain `--no-verify` is refused there and
  `GIT_MUX=1 git commit` by pre-commit. post-commit is not run at all. The one exception is a
  configured hook (see Traps). The shared branch is protected against deletion like against a
  move (see Traps for the one form let through).
- The agent hook names a directory for an invocation only when the shell is surely there when git
  runs: `git -C DIR …` and `cd DIR && git …` (unless a `||` before the `cd` made it
  conditional); `cd DIR` on its own line or before `;` when DIR is an existing literal path and no
  `if`, loop, `case`, function or `||` makes it conditional. Every doubt makes the directory
  unknown, never wrong: the command then counts as the checkout the shell is in, else as every
  checkout the text names, and the refusal says so.
- The agent hook parses every payload and fails closed: a crash, a git error or an unreadable
  payload that names an enrolled checkout exits 2, which both Claude Code and Codex honour as a
  refusal. A call that concerns no enrolled checkout exits 0, and lifecycle events never refuse
  (exit 2 on `Stop` would force the agent on). Two handlers run per call: the user-level one is
  a shell wrapper that exits 0 when the binary is missing, so other projects are never blocked;
  the project one (`.claude/settings.local.json`, exec form, `onFailure: "block"`) refuses where
  the checkout is enrolled. Claude Code runs an identical handler from two files once, which is
  why the commands differ, so every handler is idempotent per `tool_use_id`. Codex gets no
  project file and is treated as fail-open until its failure semantics are documented.
- A lease belongs to a session, a subagent's `agent_id` and the agent process (pid plus start
  time), never to the session id alone: two subagents of one session clash, and a gone agent
  frees its files. A caller with no session is judged by agent process only. A lease or shell
  log with no recorded process counts as alive, because treating it as gone would hand an
  unidentified agent's files to other sessions; `SessionEnd` releases it.
- Nothing expires on a clock. A pending shell call closes on an event only — `PostToolUse`,
  `PostToolUseFailure`, `PermissionDenied`, `Stop`, `Interrupt`, `SubagentStop`, `SessionEnd` or
  a gone agent process. A call that was cancelled or refused by a dialog degrades attribution to
  "unowned" until the session's next `Stop`, never to a wrong owner. A shell command's writes
  become its session's only when no other session was active in the same window.
- Only `git mux install` enrolls a checkout; every other command repairs the guards of one that
  is enrolled and touches nothing else. `uninstall` gives every hook file back byte for byte (a
  pre-commit it created is deleted).
- Publishing writes a journal first; every run heals an interrupted one before doing anything.

## Traps

- The binary is `git-mux`, so `git mux …` works; the shell parser treats `git mux` and
  `…/git-mux` alike, and the guard must never refuse it.
- `reference-transaction` sees a branch switch and a new worktree's `HEAD` alike, so `HEAD` moves
  are left to the agent hook; the git hook guards the branch ref and `refs/stash`.
- Empty path lists mean "nothing", never "the whole tree": `git ls-files` and `checkout` with no
  pathspec act on everything.
- A hooks directory git tracks is never edited: a note names the line to add instead. An ignored
  and generated one (husky) is rewritten on every run, so git-mux adds its line again on each
  `git mux` call; the slot copies it from the checkout, and a hook the checkout has but the slot
  lacks refuses.
- The ref guard lets through a deletion of the shared branch whose old id equals both a loose and
  a packed copy of it, because that is how git reports `pack-refs` pruning. So `git pack-refs
  --all --no-prune` followed by `git update-ref -d <branch> <its id>` passes; it takes two
  deliberate commands, and the accidental forms (`update-ref -d` without an old id, `branch -D`)
  are refused.
- A configured hook (`hook.<name>.event`, git 2.54+) runs before any hook file and cannot be
  stopped by a guard: it is the one commit or push hook that runs in the live tree on a person's
  plain commit, `git mux status` names it, and only the agent hook covers agents there. The shell
  parser is hand-written (`serde_json` is the only dependency): any word `git` or `…/git` starts a
  git invocation wherever it stands, and inline interpreter code and script files are not parsed —
  the git layer still refuses branch moves, stash and live-tree commit and push hook files.
- Every git call runs with `GIT_LITERAL_PATHSPECS=1` so `[id]` paths stay literal; what runs user
  code (hooks, `slotSetup`) or needs a glob has it removed. `slotSetup` is shell run for anyone
  who commits in the checkout, the same trust as its hooks; `selfInstall` replaces the binary, so
  only local git config may set it, never the committed `.gitmux`.
- Tests drive the real binary against real repositories, hooks and processes (`tests/*.rs` on
  the harness in `tests/common`); each test sets `HOME`, `XDG_CONFIG_HOME` and `XDG_CACHE_HOME`
  so nothing reaches the developer's agent config or slot cache, and no lease test may depend on
  an ambient `claude` or `codex` ancestor process. `GITMUX_TEST_PRE_COMMIT=1` makes a missing
  `pre-commit` a failure instead of a skip.
- This crate lives in a monorepo and is published by `git subtree split --prefix=gitmux`: it must
  not name the monorepo, its hosts or its people.
