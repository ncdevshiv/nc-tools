# nc-tools SPEC — Typed Machine API for Coding Agents

Version: 1.0 (implementation-matched)

## 1. Problem

Coding agents drive machines through the shell: untyped text-in/text-out, exit
codes carrying 1 bit of signal, hidden state (cwd, env), platform divergence,
no schema, no replay, no scoping. Every serious failure mode of agentic coding
— silent partial edits, unrecoverable mistakes, unverifiable claims,
unmeasurable efficiency — traces back to this untyped interface.

## 2. Thesis

Expose the machine as a **typed tool surface**: every operation has a JSON
schema for input, a structured result, a structured error with a
machine-readable code and remediation hint, and a journal event. The agent
never runs a shell. Any MCP-capable harness can mount the surface; any model
can drive it.

## 3. Kernel tool surface (v1)

All tools live under the `nc` server. Names are `category.action`.

### fs.*
- `fs.read` `{path, offset?, limit?}` → `{content, totalLines, truncated}`
- `fs.write` `{path, content}` → `{path, bytes, created}` — creates parent dirs;
  refuses to silently overwrite unless path already existed (event records it)
- `fs.append` `{path, content}` → `{path, bytes, created}` — appends, creating
  the file + parent dirs if missing
- `fs.copy` `{from, to, recursive?}` → `{from, to, copied}` — file or dir copy
- `fs.list` `{path, recursive?}` → `{entries:[{name,path,type,size}]}`
- `fs.stat` `{path}` → `{exists, type, size, mtimeMs}`
- `fs.mkdir` `{path, recursive?}` → `{path, created}`
- `fs.delete` `{path, recursive?}` → `{path, deleted}` (safe: refuses outside workspace root)
- `fs.move` `{from, to}` → `{from, to}`

### patch.*
- `patch.apply` `{path, edits:[{oldText, newText, expectedCount?}]}` →
  `{applied:[{index, replacements}], path}` — search/replace with exact-match
  semantics. Error `PATCH_NO_MATCH` / `PATCH_AMBIGUOUS` (when `oldText` occurs
  more than once and no `expectedCount`) carry the occurrence counts and the
  nearest-matching line numbers as remediation hints.

### search.*
- `search.grep` `{pattern, path?, glob?, maxResults?}` →
  `{matches:[{file, line, text}], total, truncated}` (regex, line-based)
- `search.files` `{pattern, path?}` → `{files:[...], total}` (glob-style)

### git.*
- `git.status` `{}` → `{branch, head, files:[{path, status}]}` (porcelain parse)
- `git.diff` `{path?}` → `{diff}` (unified diff text)
- `git.add` `{paths}` → `{added}`
- `git.commit` `{message}` → `{sha, message}`
- `git.log` `{maxCount?}` → `{commits:[{sha, message, author, date}]}`
- `git.branch` `{name?}` → `{branches:[{name, current}], current}` (list) or
  `{branch, created}` (create)
- `git.checkout` `{branch, create?}` → `{branch, created}`
- `git.push` `{remote?, branch?, setUpstream?}` → `{remote, branch, output}`
- `git.pull` `{remote?, branch?, ffOnly?}` → `{remote, branch, output}`
  (ff-only by default — no surprise merge commits)

### proc.*
- `proc.spawn` `{cmd, args, cwd?, timeoutMs?, background?}` →
  `{pid, exitCode, stdout, stderr, timedOut}` — the ONLY execution tool; it is
  typed (arg array, no shell), journaled, and timeout-bounded. Not a shell: no
  string interpolation, no pipes, no cwd tricks. Exists for compilers, test
  runners, and build tools, which are programs with structured behavior.
- `proc.list` `{filter?, maxResults?}` → `{processes:[{pid, name, memKb?}], total}`
  — the OS process table (tasklist/ps)
- `proc.kill` `{pid, force?}` → `{pid, signal, requested}` — kill by PID
  (`ERR_PROC_NOT_FOUND` / `ERR_REFUSED`)

### sys.*
- `sys.journal` `{lastN?}` → `{events:[...]}` — the agent can read its own trail
- `sys.workspace` `{}` → `{root, platform, git: bool}`

## 4. Journal

Append-only `journal.jsonl` in `<workspace>/.nc-tools/`. One JSON object per
event:

```json
{"ts": "ISO-8601", "seq": 17, "kind": "tool.call" | "tool.result",
 "tool": "patch.apply", "call": {...}, "ok": true, "result": {...},
 "error": {"code": "PATCH_NO_MATCH", "message": "...", "hint": {...}},
 "durationMs": 4}
```

Rules:
- Every tool call produces exactly one `tool.call` event and one `tool.result`.
- Journal is readable by the agent (`sys.journal`) and by the benchmark
  harness (for efficiency + behavior metrics).
- Journal survives runs; the benchmark runs use fresh workspaces but keep the
  journal as the run artifact.

## 5. Safety model

- **Workspace jail.** All path-taking tools resolve the target against the
  workspace root and refuse anything that escapes it (`ERR_PATH_ESCAPE`).
- **No silent overwrite of unknown files** without a write event; the journal
  is the audit trail.
- **proc.spawn is allowlist-free but typed**: argv array, explicit cwd, hard
  timeout, full stdout/stderr captured as data.

## 6. MCP binding

`src/mcp/server.mjs` speaks MCP stdio (JSON-RPC 2.0, protocol
revision `2024-11-05`): `initialize`, `tools/list`, `tools/call`. Every kernel
tool is exposed with its JSON schema; results are returned as structured JSON
content. Any MCP client (Claude Code, Zed, custom loops) can mount it with no
code changes.

## 7. Agent loop (reference implementation)

`src/agent/agent.mjs`: a minimal, harness-agnostic agent that receives a
system prompt, a task, and the tool surface, and loops model tool-calls to
tool execution until it emits a final answer. No terminal. Model access is an
OpenAI-compatible chat-completions endpoint (works with any provider).

## 8. Benchmark

`benchmark/` — the behavioral and efficiency layer.

- **Tasks** (`benchmark/tasks/*.json`): each has `id`, `instruction`, `setup`
  (files to create), `verify` (a JS predicate over the workspace + journal).
  Categories: `edit`, `create`, `investigate`, `refactor`, `fix`.
- **Harness** (`benchmark/harness.mjs`): for each task × model: fresh
  workspace → setup → run agent → run verifier → record transcript JSON.
- **Metrics** per run: solved (verifier verdict), tool calls, tokens
  (prompt+completion from API usage), wall time, error-tool-call ratio.
- **Verifier is real**: it inspects the resulting workspace with the same
  kernel tools and returns pass/fail with evidence. No self-report.

## 9. What v1 deliberately excludes

Virtualization/rollback, driver synthesis, multi-ecosystem drivers — the
vertical slice proves the loop: typed tools → journal → benchmark → real
numbers.
