# nc-tools — The Typed Machine API

A terminal-free machine interface for coding agents: every machine operation the
terminal used to provide (files, git, processes, search, patches, builds) is a
typed tool with structured inputs, structured outputs, structured errors, and a
full event journal.

**Thesis:** the terminal is the machine's *untyped* API. Replace it with a typed
one and agents become verifiable, replayable, measurable, and safer — without
changing their model or harness.

## Components

| Path | What it is |
|---|---|
| `src/kernel/` | The typed tool surface (fs, git, patch, search, process) + journal + snapshots |
| `src/agent/` | A terminal-free agent loop that operates the kernel via tool calls |
| `src/mcp/` | MCP stdio server exposing the kernel to any MCP-capable agent |
| `benchmark/` | Harness, task suite, verifier, and results for terminal-free runs |
| `tools/audit.mjs` | Repository audit: scans for stubs/TODOs/mocks/fakes/placeholders |

## Verified outcomes

See `benchmark/results/` for real agent runs (JSON transcripts with per-run
verdicts) and `docs/RESULTS.md` for the summary. Every claim in the docs is
reproducible with the listed commands.

## Principles

1. **Typed in, typed out.** Tools return structured results and structured
   errors with machine-readable remediation hints. No stdout scraping.
2. **Everything is journaled.** Every tool call and result is an event in
   `journal.jsonl`. Sessions are replayable and auditable.
3. **Mutations are deliberate.** Writes go through validated patches
   (search/replace blocks with occurrence counts). No silent overwrites.
4. **No shell.** The agent has no `exec`, no bash, no terminal. If a task
   requires the shell, that is a driver to build, not an escape hatch to use.
