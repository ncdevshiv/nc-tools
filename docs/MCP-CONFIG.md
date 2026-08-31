# Connecting any MCP agent to nc-tools

The kernel is a standard MCP **stdio** server (`src/mcp/server.mjs`). Any
MCP-capable agent — ZCode, Claude Desktop/Cli Code, Zed, Cursor-style MCP
hosts, custom loops — can mount it with the config below. No code changes
needed on the agent side; the protocol is MCP `2024-11-05`.

## Ready-to-paste config

### ZCode (`config.json` → `mcp.servers`)

```json
{
  "mcp": {
    "servers": {
      "nc-tools": {
        "type": "stdio",
        "command": "node",
        "args": ["F:/nc-tools/src/mcp/server.mjs", "F:/nc-tools"],
        "env": {
          "NCTOOLS_MCP_IDLE_MS": "1800000"
        },
        "enabled": true
      }
    }
  }
}
```

- `args[1]` = the workspace root the kernel is jailed to (the only files it
  can read/write plus everything else scoped there).
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
      "command": "node",
      "args": ["F:/nc-tools/src/mcp/server.mjs", "F:/nc-tools"],
      "env": { "NCTOOLS_MCP_IDLE_MS": "1800000" }
    }
  }
}
```

### Generic MCP host (any tool that takes command+args)

```
command: node
args:    F:/nc-tools/src/mcp/server.mjs F:/nc-tools
env:     NCTOOLS_MCP_IDLE_MS=1800000
```

## What the agent sees

- `tools/list` → **40 typed tools**: `fs.*`, `patch.apply(Many)`,
  `search.grep/files/semantic`, `git.*`, `proc.spawn/start/status/readOutput/stop`,
  `test.run`, `pkg.*`, `net.http/probePort`, `env.*`, `sys.snapshot/rollback/
  listSnapshots/journal/workspace`, `batch.execute`.
- Every result is structured JSON; every failure is
  `{error: {code, message, hint}}` — the agent never scrapes stdout.
- `sys.journal` lets the agent read its own trail; `sys.snapshot`/`rollback`
  make risky edits reversible.
- `search.semantic` needs `@xenova/transformers` installed (it is, in
  `F:/nc-tools/node_modules`); first use downloads the local MiniLM model.
  Set `NCTOOLS_MODEL_CACHE` to a shared dir to avoid re-downloads across
  servers.

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
- Agents *working on different tasks* → separate workspaces
  (`server.mjs <workspaceA>` / `<workspaceB>`) — zero contention, each gets
  its own journal.
- Agents *collaborating on one workspace* → same workspace, one shared
  journal, file-level writes are atomic (each `fs.write` is a single syscall
  of one file).

## Direct binding (no MCP host needed)

```js
import { Kernel } from './src/kernel/kernel.mjs';
const k = new Kernel('F:/nc-tools');
const out = await k.call('search.grep', { pattern: 'ERR_' });
console.log(out.result);
```

## Quick check

```bash
node F:/nc-tools/src/mcp/server.mjs F:/nc-tools
# other terminal:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' | node F:/nc-tools/src/mcp/server.mjs F:/nc-tools
```
