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

## How to re-run

```bash
node tools/crossaudit.mjs            # audits the repo
node tools/crossaudit.mjs F:/some/dir  # audits any workspace via its MCP server
```

Status after fixes: **58/58 unit tests, 14/14 conformance, 20/20 cross-audit,
audit-suite clean.**
