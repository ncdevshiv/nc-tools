# Tool-Surface Gap Analysis — what's missing and why it matters

Date: 2026-08-31 (wave 14). Evidence: `node tools/usage-audit.mjs` over all
219 journals on disk (218 benchmark runs + dogfooding; 4,703 tool calls),
plus the golden tool list as the coverage baseline.

## What the data says

### Frequency (top of 4,703 calls)

| Tool | Calls | Share |
|---|---|---|
| proc.spawn | 2,135 | 45.4% |
| fs.read | 464 | 9.9% |
| search.grep | 420 | 8.9% |
| patch.apply | 316 | 6.7% |
| fs.list | 290 | 6.2% |
| fs.stat | 204 | 4.3% |
| fs.write | 100 | 2.1% |
| test.run | 96 | 2.0% |

`proc.spawn` at 45% is not a shell-escape problem — it's agents running
`node`/`npm`/`git`/`pytest` through the one generic door. Every one of those
calls is untyped at the surface level (free-form argv), which is exactly what
the typed-API thesis says should shrink.

### Never called (10/48)

`fs.move, git.add, git.commit, git.checkout, git.pull, pkg.add, pkg.runScript,
env.get, sys.rollback, sys.journal` — zero uses in 4,703 calls. Two distinct
reasons: agents don't know the tool exists (description/habit gap) or the
description doesn't say when to prefer it over `proc.spawn git …`.

### Attempted-but-missing (the sharpest signal — 7 occurrences)

Agents invented tools that don't exist: `fs_stat` ×3, `fs_list` ×2 (models
that reject dotted names and guess underscored ones), `git__status`,
`sys__workspace`. They were *reaching for tools we have* — the wire-name
mapping, not the surface, failed them.

### Error profile

`ERR_BAD_PATH` 23, `ERR_FLAKY` 20 (chaos harness), `PATCH_NO_MATCH` 18,
`ERR_PATH_ESCAPE` 15, `ERR_NOT_FOUND` 13, `ERR_UNKNOWN_TOOL` 7. The
top-of-table errors are path-argument mistakes — agents composing paths from
memory instead of from `fs.list` output. `search.semantic`: 16 calls (0.3%)
— the headline tool is nearly unused, and the head-to-head showed even when
used, models don't trust the ranking.

## Gaps to close (ranked: demand-evidence × thesis-fit)

### Tier 1 — real demand, direct thesis fit (add)

1. **`proc.runScript` / typed project scripts.** 45% of all calls are
   proc.spawn; a large share are `npm run x` / `node x.js` / `pytest`. A
   typed runner that knows cwd, exit codes, and structured output would move
   the biggest traffic class onto the typed surface. (`test.run` already
   proves the pattern works — it's the model to copy; extend its idea to
   arbitrary project scripts with allowlisted interpreters.)
2. **`search.replace` (project-wide search & replace).** Agents currently do
   grep → read → patch per file. The #1 refactor flow is 3 calls per file;
   one typed tool (regex with validation, per-file result, dryRun flag)
   collapses it and is safer than an agent looping patch.apply.
3. **`fs.readRange` (offset/limit on large files).** fs.read on big files
   burns tokens; agents already hack around this with `proc.spawn head`.
   Cheap to build on the existing reader; unblocks agents from the shell for
   large-file inspection.

### Tier 2 — real demand, needs design care

4. **Wire-name aliases** — *implemented in wave 14*: `fs_stat` and the
   double-underscore wire form (`git__status`, what agent loops emit for
   providers that forbid dotted names) now resolve at kernel dispatch in both
   implementations (exact → `__`→`.` → `_`→`.`), locked by a conformance case
   and documented in PROTOCOL §6.1. This closes all 7 observed
   `ERR_UNKNOWN_TOOL` failures. Remaining alias work: none.
5. **`git.diff` for staged/unstaged split + `git.commit` amend** — git tools
   exist but are unused because descriptions don't say when to prefer them
   over `proc.spawn git …`. Fix is mostly description work plus one amend
   capability agents asked for by trying `git.commit --amend` argv.
6. **`proc.watch`** — long-running process output streaming with a cursor.
   `proc.start/readOutput` exist, but polling UX is clumsy; the 44+68+22
   calls to start/status/readOutput show agents do manage, so this is
   ergonomics, not a hole.

### Tier 3 — do NOT add (evidence says no)

- **A shell/exec tool.** The 45% proc.spawn share is the argument *for*
  typed wrappers, not for a shell. Every ladder wave showed the typed arm
  winning exactly on stateful/multi-tool tasks; a shell would erase the
  differentiator and the journal's typed structure.
- **More snapshot/rollback surface.** `sys.rollback` unused (12 snapshots
  taken, 0 rollbacks) — the gap is prompting ("snapshot before risky edits"),
  not capability.
- **Docker/container tools.** Zero attempts in any journal; no agent ever
  reached for containers. Add when there's demand evidence.

## The adoption gap is a prompt problem, not a tool problem

`search.semantic` (0.3%) and the never-called list show the surface already
exceeds what models spontaneously use. The cheapest wins are: (a) wire-name
aliases (Tier-2 #4), (b) `search.grep` responses suggesting `search.semantic`
for conceptual queries (the response can carry a `suggest` field — structured,
untyped-free), (c) descriptions that name the concrete beat ("use BEFORE
grep when you know what the code does, not what it says").