# nc-tools — The Typed Machine API

A terminal-free machine interface for coding agents: every machine operation the
terminal used to provide (files, git, processes, search, patches, builds) is a
typed tool with structured inputs, structured outputs, structured errors, and a
full event journal.

**Thesis:** the terminal is the machine's *untyped* API. Replace it with a typed
one and agents become verifiable, replayable, measurable, and safer — without
changing their model or harness.

## Components

The **primary implementation is Rust**: `cargo build --release -p nct-mcp`
produces one static `nc-tools-mcp` binary — no Node, no npx, no PATH shims.
The JS implementation is the **frozen conformance oracle** under `oracle/`:
the golden spec and the black-box conformance suite grade every other
implementation against it.

| Path | What it is |
|---|---|
| `rust/` | **Primary implementation**: 8 crates; `cargo build --release -p nct-mcp` → single static `nc-tools-mcp` binary, same 48-tool protocol |
| `oracle/` | JS reference implementation, archived: kernel, MCP stdio server, agent loop. Frozen oracle for conformance; runs via `node oracle/mcp/server.mjs` |
| `conformance/golden/` | Frozen 48-tool descriptor export (`tools/golden.mjs` regenerates; changes must be deliberate) |
| `benchmark/` | Harness, task suite, verifier, and results for terminal-free runs |
| `tools/audit.mjs` | Repository audit: scans for stubs/TODOs/mocks/fakes/placeholders (JS **and** Rust source) |

## Verified outcomes

See `benchmark/results/` for real agent runs (JSON transcripts with per-run
verdicts) and `docs/RESULTS.md` for the summary. Every claim in the docs is
reproducible with the listed commands.

Benchmark arms:
- `node benchmark/harness.mjs` — terminal-free runs (typed kernel tools only)
- `node benchmark/compare.mjs` — head-to-head: same tasks/models/verifier with
  typed tools vs a single bash tool. Current result: capability parity,
  bash ~40% cheaper on short tasks; the differentiators (recovery, safety,
  replay, chaos injection) need harder tasks — that is the benchmark roadmap.

## Principles

1. **Typed in, typed out.** Tools return structured results and structured
   errors with machine-readable remediation hints. No stdout scraping.
2. **Everything is journaled.** Every tool call and result is an event in
   `journal.jsonl`. Sessions are replayable and auditable.
3. **Mutations are deliberate.** Writes go through validated patches
   (search/replace blocks with occurrence counts). No silent overwrites.
4. **No shell.** The agent has no `exec`, no bash, no terminal. If a task
   requires the shell, that is a driver to build, not an escape hatch to use.
