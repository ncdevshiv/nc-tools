# Phase 2 — Rust-Only Live System

> **Historical record** — written when the repo still carried the archived JS
> oracle implementation. Numbers are as measured at the time; the current
> repo is Rust-only (see the README for the live state and tool count).


**Status:** Historical wave report (2026-09). superseded by the current
Rust-only layout described in the README; kept for the audit trail.

## Goal

Make `nc-tools` a Rust-only live system: freeze the JS oracle as archive,
implement 10 new fully-real tools (zero stubs/mocks), regenerate the golden
from the Rust binary, port the test harness, run a real benchmark, and deliver
this report.

## What changed

1. **Golden is regenerated from the Rust binary.** `tools/golden.mjs` now
   invokes the built Rust binary (`nc-tools-mcp --dump-tools <out>`) instead of
   importing the JS descriptors. `nct-mcp/src/main.rs` gained a `--dump-tools`
   flag for this. `conformance/golden/tools.json` now carries
   `"generatedFrom": "nct-mcp (build_kernel)"` and **57 tools**.
2. **JS oracle frozen.** The oracle is no longer the source of truth; the Rust
   kernel's `descriptors()` is. The golden parity test
   (`nct-mcp/tests/parity.rs`) now freezes the Rust surface and fails on any
   un-regenerated change.
3. **10 new tools implemented and registered** (see below), plus a cargo driver
   added to `test.run`.

## New tools

| Tool | Crate / file | What it does |
|------|--------------|--------------|
| `fs.readRange` | `nct-fs/src/fs_tools.rs` | Byte-offset windowed read; returns `byteOffset`/`byteLength`/`nextByteOffset`/`eof`/`startLine`, trims to a UTF-8 boundary. |
| `fs.tree` | `nct-fs/src/fs_tools.rs` | Bounded directory tree (depth/entry caps, skips `.git`/`node_modules`/`target`/`dist`/`.nc-tools`, dirs-first). Returns structured entries + ASCII rendering. |
| `search.replace` | `nct-fs/src/search.rs` | Regex search/replace across files, `dryRun` by default; `$0..$9` backrefs (`$$`=literal `$`); skips binaries + generated dirs. |
| `code.symbols` | `nct-fs/src/symbols.rs` | Fast lexical scanner for Rust/JS/TS/TSX/JSX/Python; function/struct/enum/trait/impl/mod/class/interface/type/def with line numbers; `glob`/`kinds` filters, `maxResults` cap. |
| `text.diff` | `nct-fs/src/diff.rs` | Minimal unified diff (LCS edit script, git-style `@@` hunks, context lines); whole-body fallback for very large/distant inputs. |
| `proc.runScript` | `nct-proc/src/lib.rs` | Run an interpreted script (js/python/shell/powershell/batch) from a file or inline source, with a hard timeout. |
| `proc.watch` | `nct-proc/src/lib.rs` | Managed watcher that re-launches a long-lived command when a file/dir changes; integrated with `proc.status/readOutput/stop`. |
| `git.blame` | `nct-git/src/lib.rs` | Per-line blame (`git blame --line-porcelain`) with commit/author/timestamp/content, optional `-L start,end`. |
| `sys.doctor` | `nct-mcp/src/doctor.rs` | One-call self-diagnostics: tool inventory, config limits, session env, journal stats; `deep=true` also probes runtime (writable base, cargo). |
| `test.run` (cargo) | `nct-proc/src/test.rs` | `framework: "cargo"` runs `cargo test` and returns structured passed/failed/skipped counts + failure names. |

Total resident tool surface: **57** (was 48).

## Verification

- `cargo check --workspace` — clean (all family crates + binary).
- `cargo test --workspace --lib` — all pass (incl. `nct-core` schema test).
- `cargo test -p nct-mcp --test parity` — **PASS** (Rust surface == regenerated golden, 57 tools).
- Live smoke tests against the debug binary (via MCP `tools/call`):
  - `text.diff` on two LF files → correct minimal hunk
    `@@ -1,3 +1,4 @@ / line1 / -line2 / +CHANGED / line3 / +line4`.
  - `code.symbols` on `nct-fs/src/fs_tools.rs` → 68 symbols (struct/fn/impl) with line numbers.
  - `fs.tree` on `nct-core` → nested tree + structured entries, skipped `.git`/`target`.
  - `proc.runScript` (inline JS via node) → exit 0, `stdout: runscript-ok`.
  - `git.blame` on a tracked Rust file → per-line commit/author/content.
  - `proc.watch` (start → status → stop in one session) → handleId `h1`, clean lifecycle.

## Known limitations

- `text.diff` uses an exact LCS edit script (O(n·m)); inputs with
  `len_a * len_b > 5_000_000` fall back to a whole-body diff. A true Myers
  implementation is a follow-up (the initially-ported Myers backtracking was
  removed because it produced a non-minimal script).
- `proc.watch` keeps the handle table in-process (same session); a stale
  `proc.status` after the server restarts reports `ERR_UNKNOWN_HANDLE` — this is
  the intended long-lived-server contract.
- `proc.runScript` shell language maps through `cmd /c` on Windows and `sh`
  on Unix; it is not a POSIX shell on Windows.
- `sys.doctor` `deep` probes are best-effort (a read-only workspace is reported,
  not fatal).

## Remaining work

- Port `conformance/cases.mjs` to drive the Rust binary and add cases for the
  new tools; run the conformance suite against the Rust server.
- Update `tools/crossaudit.mjs` and `tools/audit.mjs` hygiene scans for the
  Rust-primary layout with the JS implementation archived.
- Port `tests/` to drive the Rust binary (some in-process-hook tests cannot
  cross the MCP boundary and need a driver harness).
- Run the real-world benchmark (real clone, cargo builds, git, patch+rollback,
  semantic search) and record results in `docs/RESULTS.md`.
- Commit the phase.

