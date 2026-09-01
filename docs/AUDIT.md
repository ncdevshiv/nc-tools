# Cross-Audit Report — nc-tools audited through its own MCP server

Date: 2026-08-31 · Tool: `node tools/crossaudit.mjs` · Result: **20/20 PASS**
(re-run after fixes; the pre-fix run was 15/17 with 2 real defects found)

The audit drives `src/mcp/server.mjs` as an opaque stdio process — the exact
same protocol path a client uses — and verifies the codebase against the
contract it publishes. Every check is a real tool call; no local file poking.
A fresh temp workspace is used for behavior checks so the repo is never
mutated.

## Connection check: your configured MCP server

- Config (`config.json` → `mcp.servers.nc-tools`): `node F:/nc-tools/src/mcp/server.mjs F:/nc-tools`, `NCTOOLS_MCP_IDLE_MS=1800000`.
- zcode log events: `connect.started → connection.created → lease.acquired → server.connected`, **no `mcp.server.failed` for nc-tools**. (The `mcp.server.failed` warnings in the log are for the *remote* `document-skills:image_search` server — "official MCP rejected the current credential" — unrelated to nc-tools.)
- Direct handshake probe: `initialize` → `{"name":"nc-tools","version":"0.1.0"}`, protocol `2024-11-05`, `tools/list` → 40 tools. **Connected and healthy.**

## Checks (all through MCP)

| # | Check | Result |
|---|---|---|
| 1 | protocol handshake (2024-11-05, serverInfo nc-tools) | PASS |
| 2 | tool count = 40 | PASS |
| 3 | every tool has JSON-schema inputSchema + description | PASS |
| 4 | tool names match PROTOCOL.md list | PASS |
| 5 | fs.write + fs.read round-trip (numbered content) | PASS |
| 6 | ERR_NOT_FOUND with nearestExisting hint | PASS |
| 7 | ERR_PATH_ESCAPE for traversal | PASS |
| 8 | patch.apply exact-match + PATCH_NO_MATCH with candidate lines | PASS |
| 9 | search.grep returns file/line/text | PASS |
| 10 | ERR_UNKNOWN_TOOL (no bash/shell tool exists) | PASS |
| 11 | proc.spawn typed argv + exit code capture | PASS |
| 12 | journal: every result's callSeq resolves to a call (no orphans) | PASS |
| 13 | sys.snapshot + sys.rollback removes post-snapshot files | PASS |
| 14 | batch.execute per-item ok/failed, one bad item doesn't abort | PASS |
| 15 | git.status on a real repo | PASS |
| 16 | git.add + git.commit round-trip (real sha returned) | PASS |
| 17 | no TODO/FIXME/XXX markers in src/ | PASS |
| 18 | no placeholder/not-implemented markers in src/ | PASS |
| 19 | docs claim "40 tools" matches implementation | PASS |
| 20 | PROTOCOL.md tool-count statement consistent with implementation | PASS |

## Defects found and fixed by this audit

1. **`search.grep` crashed on file paths (ENOTDIR).** Calling
   `search.grep {pattern, path: 'docs/PROTOCOL.md'}` — a path to a single
   file, not a directory — threw `ENOTDIR: not a directory, scandir ...`.
   The walker assumed `path` was a directory. Fix: accept both file and
   directory paths (also added nearest-file hints on missing paths, and the
   hint shape was normalized). **Regression tests added**
   (`tests/p0-kernel.test.mjs`: file-path grep passes; missing file returns
   structured ERR_NOT_FOUND with sibling hints). Found because the audit's
   doc cross-check greps a single file — a realistic agent usage (grep one
   file you just read).

2. **`docs/PROTOCOL.md` stale tool count.** The spec said "39 tools" (twice),
   implementation and conformance had been 40 since wave 4. Fixed to 40.

3. **Hint shape inconsistency (pre-existing, caught during fix #1).** One
   error path nested the hint as `{hint: {...}}` while every other error had
   `{hint}` directly. Normalized.

## Post-wave-8 fix wave (2026-08-31)

Wave 9 (uncommitted at time of writing) — defects found by re-auditing through
the live MCP, all fixed + regression-tested (`tests/auditfixes.test.mjs`):

1. **Jail bypass via symlinks/junctions (high).** The path jail checked paths
   lexically only; a junction inside the workspace pointed at `C:\Windows` and
   `fs.read` followed it. `paths.mjs` now resolves the real on-disk location
   (deepest existing ancestor) and re-verifies it stays inside the root; all
   walkers (`fs.list`, `search.grep/files`, `search.semantic`,
   `sys.snapshot/rollback`) skip reparse points that leave the workspace.
2. **Case-sensitive jail compare (Windows).** `F:/NC-TOOLS/…` and
   `f:/nc-tools/…` were falsely rejected as escapes; comparisons are now
   case-folded on win32.
3. **`search.files` crashed `ERR_INTERNAL ENOTDIR` on single-file paths** — the
   wave-8 fix covered `search.grep`; `search.files` now handles files too.
4. **`test.run` false green.** Default patterns missed `tests/` and reported
   `{passed: 0, failed: 0, exitCode: 0}`; directories are now expanded to
   globs and zero discovered tests return `ERR_NO_TESTS` instead of a silent
   pass. `package.json` `test` script fixed (`node --test tests/` fails on
   Node 24 — dirs are not descended; glob form runs the suite).
5. **`NCTOOLS_MCP_IDLE_MS=0` killed the server.** A 0ms idle timer exited after
   the first request (docs told users to set `"0"` to disable). Non-positive /
   non-numeric values now disable the timer; docs corrected.
6. **`search.semantic` path not jailed** (`join(root, path)` without
   `inWorkspace`) — `path: ".."` would have indexed the whole drive; now
   validated before the model loads.
7. Misc: batch/call accept MCP-style underscored names (`fs_stat` → `fs.stat`);
   `proc.spawn/start` enforce their configured time bounds in the handler
   (schema caps are bypassable via `batch.execute`); `git.status` hides only
   the `.nc-tools` directory; `ERR_NO_TESTS` added to the taxonomy.

## Wave 11 (2026-08-31): jail removed — global tool system

By design decision — the kernel is a machine-wide tool surface for remote
agents working in parallel across many projects — the workspace jail was
removed: absolute paths are accepted for any location, relative paths resolve
against the base dir given at startup (`argv[2] / NCTOOLS_WORKSPACE / cwd`),
and `ERR_PATH_ESCAPE` no longer exists as an observable error. `git.*` gained a
`repo` param and `pkg.*` a `dir` param so they operate outside the base;
recursive walks keep skipping symlinks/junctions, and `fs.delete` still
refuses the base dir and filesystem roots. The wave-9 hardening
(case-folding, reparse-point detection) was not wasted — it is now the
cycle-safety mechanism.

## How to re-run

```bash
node tools/crossaudit.mjs            # audits the repo
node tools/crossaudit.mjs F:/some/dir  # audits any workspace via its MCP server
```

Status after fixes: **73/73 unit tests, 14/14 conformance, 20/20 cross-audit,
audit-suite clean.**

## Wave 10 — terminal-control gap fill

57 tools (was 48). New: `git.branch/checkout/push/pull`, `proc.list`
(OS process table)/`proc.kill` (kill by PID), `fs.copy`, `fs.append`.
Cross-audit now derives tool-count assertions from its expected list instead
of hardcoding 40; conformance and MCP tests assert 57. Also fixed: managed
`proc.start` processes left their `maxDurationMs` timer armed after exit,
holding the event loop (and process) alive for the full duration.
