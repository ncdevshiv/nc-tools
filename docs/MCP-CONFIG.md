# Connecting any MCP agent to nc-tools

The kernel ships as a standard MCP **stdio** server: one static Rust executable,
no runtime dependencies, no PATH shims, instant cold start.

Build it once from the repo root:

```bash
cargo build --release -p nct-mcp
# → target/release/nc-tools-mcp       (Linux/macOS)
# → target/release/nc-tools-mcp.exe   (Windows)
```

Recommended: install it to a stable path that survives `cargo clean`:

```bash
npm run install:bin     # copies the release binary to ~/.local/bin and verifies it
```

Any MCP-capable agent — ZCode, Claude Desktop, Zed, Cursor-style MCP hosts,
custom loops — can mount it with the config below.

## Ready-to-paste config

Replace `/path/to/nc-tools-mcp` with your built (or installed) binary path and
`/path/to/workspace` with the directory the kernel should anchor.

### ZCode (`config.json` → `mcp.servers`)

```json
{
  "mcp": {
    "servers": {
      "nc-tools": {
        "type": "stdio",
        "command": "/path/to/nc-tools-mcp",
        "args": ["/path/to/workspace"],
        "env": {
          "NCTOOLS_MCP_IDLE_MS": "1800000"
        },
        "enabled": true
      }
    }
  }
}
```

- `args[0]` = the base dir the kernel anchors relative paths, journal and
  snapshots to. Paths are NOT restricted to it: absolute paths work anywhere
  on the machine (global tool system).
- `NCTOOLS_MCP_IDLE_MS` = idle auto-sleep (default 30 min): after that long
  with no requests, the server exits. Standard MCP clients restart a stdio
  server when the next call arrives, so the agent effectively "sleeps"
  between uses and wakes on demand. Set `"0"` to disable the idle timer
  entirely (the server then stays up until the client disconnects).

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "nc-tools": {
      "command": "/path/to/nc-tools-mcp",
      "args": ["/path/to/workspace"],
      "env": { "NCTOOLS_MCP_IDLE_MS": "1800000" }
    }
  }
}
```

### npx-only hosts (Qwen Edit and friends)

Some hosts only accept `npx`/`uvx` as the launch command. This repo's
`package.json` exposes a `bin` shim (`bin/nc-tools-mcp.mjs`) that locates and
execs the static binary — from the repo's `target/release/`, then
`~/.local/bin`. After `npm install` (or a global install of this package),
`npx nc-tools-mcp /path/to/workspace` works.

### Generic MCP host (any tool that takes command+args)

```
command: /path/to/nc-tools-mcp
args:    /path/to/workspace
env:     NCTOOLS_MCP_IDLE_MS=1800000
```

## What the agent sees

- `tools/list` → **60 typed tools**: `fs.*` (incl. `append`/`copy`/`readRange`/`tree`),
  `patch.apply(Many)`, `search.grep/files/replace/semantic`, `code.symbols`,
  `text.diff`, `git.*` (incl. `branch/checkout/push/pull/blame`),
  `proc.spawn/start/status/readOutput/stop/runScript/watch`
  (incl. `list`/`kill`), `test.run`, `pkg.*`,
  `net.http/probePort/fetch/robots/search`, `env.*`,
  `sys.snapshot/rollback/listSnapshots/journal/workspace/doctor`, `batch.execute`.
- Every result is structured JSON; every failure is
  `{error: {code, message, hint}}` — the agent never scrapes stdout.
- `sys.journal` lets the agent read its own trail; `sys.snapshot`/`rollback`
  make risky edits reversible.
- `search.semantic` is fully self-contained in the Rust binary (candle +
  all-MiniLM-L6-v2, pure Rust, local); the first use downloads the model into
  the cache dir. Set `NCTOOLS_MODEL_CACHE` to a shared dir to avoid
  re-downloads across servers.

## Parallel agents: yes, and here's what must be true

Multiple nc-tools servers (multiple agents) can run **simultaneously on the
same workspace**. Two facts make this safe, and both are tested
(`tests/parallel.test.mjs`):

1. **Journal writes are cross-process serialized.** Every event append takes
   an exclusive lock (`.nc-tools/.journal.lock`); the file is never torn even
   when two agents journal concurrently. Tested with two live server
   processes, 40 interleaved calls, asserting every JSONL line parses and
   every `callSeq` resolves.
2. **Errors stay structured under racing.** If agent A reads a file agent B is
   mid-creating, it gets a normal `ERR_NOT_FOUND` — the race is a recoverable
   result, not a crash or corruption.

Recommended per-agent layout anyway:
- Agents *working on different tasks* → separate workspaces — zero
  contention, each gets its own journal.
- Agents *collaborating on one workspace* → same workspace, one shared
  journal, file-level writes are atomic.

## Quick check

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' | /path/to/nc-tools-mcp /tmp
```

Expect an `initialize` response with `serverInfo.name == "nc-tools"`, then a
`tools/list` with 60 entries.
