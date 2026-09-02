# nc-tools — the Typed Machine API for coding agents

A terminal-free machine interface: every operation the terminal used to provide
(files, search, patches, git, processes, tests, packages, network) is a **typed
tool** with structured inputs, structured outputs, structured errors, and a
full append-only event journal. One static Rust binary speaks MCP over stdio.

**Thesis:** the terminal is the machine's *untyped* API. Replace it with a typed
one and agents become verifiable, replayable, measurable, and safer — without
changing their model or harness.

## Quick start

Requires [Rust](https://rustup.rs) (stable) and Node ≥ 20 (for the test/dev
harnesses only — the kernel itself has zero runtime dependencies).

```bash
git clone https://github.com/ncdevshiv/nc-tools.git
cd nc-tools
cargo build --release -p nct-mcp        # → target/release/nc-tools-mcp(.exe)
npm run conform                          # 27 protocol conformance cases, black-box
```

Mount it in any MCP client (ZCode, Claude Desktop, Cursor-style hosts, custom
loops) — see [docs/MCP-CONFIG.md](docs/MCP-CONFIG.md). Point the command at
`target/release/nc-tools-mcp` with your workspace root as the only argument, or
run `npm run install:bin` to copy it to `~/.local/bin` and use that stable path.

## What's in the box

| Path | What it is |
|---|---|
| `crates/` | The Rust workspace — 8 crates; `nct-mcp` is the MCP stdio server, `nct-agent` is a headless agent loop embedding the kernel |
| `conformance/` | Language-neutral protocol cases (`conformance/cases.mjs`) + the frozen golden tool export (`conformance/golden/tools.json`) |
| `tests/` | Node test suite driving the binary as a black box over MCP stdio |
| `bench/` | Task suite, verifier validator, and agent-run harness (real verifiers, journaled transcripts) |
| `tools/` | Conformance runner, cross-audit, stub-audit, golden exporter, kernel client |
| `docs/` | Protocol spec, MCP client config, internet-tool design, audit trail |

## The tool surface (60 typed tools)

- `fs.*` — read/readMany/readRange, write/writeMany, append, copy, list, tree,
  stat, mkdir, delete, move
- `patch.apply/applyMany` — search/replace edits with exactness guards
- `search.grep/files/replace/semantic`, `code.symbols`, `text.diff`
- `git.status/diff/add/blame/commit/log/branch/checkout/push/pull`
- `proc.spawn/start/status/readOutput/stop/list/kill/runScript/watch` — argv-typed, no shell
- `test.run` (node, pytest), `pkg.add/list/scripts/runScript`
- `net.http/probePort/fetch/robots/search` — see [docs/INTERNET-TOOLS.md](docs/INTERNET-TOOLS.md)
- `env.get/set/list`, `sys.snapshot/rollback/listSnapshots/journal/workspace/doctor`, `batch.execute`

The frozen machine-readable export is [`conformance/golden/tools.json`](conformance/golden/tools.json)
(regenerate with `npm run golden`; changes must be deliberate). The full
contract — error taxonomy, journal schema, transport bindings — is
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Principles

1. **Typed in, typed out.** Tools return structured results and structured
   errors with machine-readable remediation hints. No stdout scraping.
2. **Everything is journaled.** Every tool call and result is an event in
   `<workspace>/.nc-tools/journal.jsonl`. Sessions are replayable and auditable.
3. **Mutations are deliberate.** Writes go through validated patches
   (search/replace blocks with occurrence counts). No silent overwrites.
4. **No shell.** There is no `exec`/bash tool. `proc.spawn` is argv-typed.
   If a task requires a shell, that is a tool to design, not an escape hatch.

## Verification gates

Every claim in this repo is machine-checked:

```bash
cargo test                       # Rust unit + integration suites
npm run test:node                # black-box protocol/behavior suite over MCP stdio
npm run conform                  # language-neutral conformance (27 cases)
npm run crossaudit               # audits THIS repo through its own MCP server
npm run audit                    # stub/TODO/mock/placeholder scan (exit 1 on findings)
npm run verify:verifiers         # bench verifiers: untouched→fail, solved→pass, wrong→rejected
```

## License

MIT
