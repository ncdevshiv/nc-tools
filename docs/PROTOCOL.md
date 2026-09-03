# nc-tools Kernel Protocol v1 — language-neutral portability spec

This document is the contract for *any* reimplementation of the nc-tools
kernel — Rust, Go, Python, Java, or a different JS runtime — that claims to
be a compatible nc-tools implementation. The primary (and only shipped)
implementation is the Rust workspace in this repo (`crates/`). A port must pass
the conformance suite (`tools/conformance.mjs`) without modification.

## 1. Scope

`docs/PROTOCOL.md` defines: (a) the machine API surface (tools + schemas),
(b) the session (workspace, env, process handles), (c) the journal format,
(d) the error taxonomy, (e) the transport bindings, and (f) conformance
requirements. It deliberately does NOT define implementation language,
internal architecture, or data structures beyond the observable contract.

## 2. Terms

- **kernel** — the runtime that owns a workspace and executes tool calls.
- **tool** — a named operation `category.action` with JSON input and output.
- **session** — one kernel instance bound to one base directory (anchor
  for relative paths, journal, snapshots), with session state: environment
  overrides, process handles, snapshots.
- **journal** — the append-only event log (`<workspace>/.nc-tools/journal.jsonl`).
- **call/result pairs** — every tool call produces exactly one `tool.call`
  event and one `tool.result` event. The result's `callSeq` references the
  call's `seq`. (See schema below.)

## 3. Tool surface (must be 84 tools; schema in the Rust kernel `crates/`, frozen export at `conformance/golden/tools.json`)

- `fs.read`, `fs.readMany`, `fs.readRange`, `fs.write`, `fs.writeMany`,
  `fs.append`, `fs.copy`, `fs.list`, `fs.tree`, `fs.stat`, `fs.mkdir`,
  `fs.delete`, `fs.move`
- `patch.apply`, `patch.applyMany`
- `search.grep`, `search.files`, `search.replace`, `search.semantic`
- `code.symbols` (Rust/JS/TS/TSX/JSX/Python symbols)
- `text.diff` (minimal unified diff)
- `git.status`, `git.diff`, `git.add`, `git.blame`, `git.commit`, `git.log`,
  `git.branch`, `git.checkout`, `git.push`, `git.pull`
- `proc.spawn`, `proc.start`, `proc.status`, `proc.readOutput`, `proc.stop`,
  `proc.list`, `proc.kill`, `proc.runScript`, `proc.watch`
- `test.run` (frameworks: `node`, `pytest`)
- `pkg.add`, `pkg.list`, `pkg.scripts`, `pkg.runScript`
- `net.http`, `net.probePort`, `net.fetch`, `net.robots`, `net.search`, `net.cite`, `net.verify`, `net.research`, `net.contradict`
  (wave W-Net-1: agent-grade fetch+extract, robots/llms.txt, keyless federated
  search with local neural rerank — see docs/INTERNET-TOOLS.md; net.research is
  the multi-hop conductor, net.contradict the anti-confirmation-bias check)
- `env.get`, `env.set`, `env.list`
- `sys.snapshot`, `sys.rollback`, `sys.listSnapshots`, `sys.snapshotDiff`,
  `sys.journal`, `sys.workspace`, `sys.doctor`
- `batch.execute`

Behavioral invariants every implementation MUST honor:

1. **Path model (no jail).** Relative paths resolve against the base
   directory; absolute paths are accepted for ANY location on the machine
   (global tool system — remote agents may work in different projects in
   parallel, and `git.*`/`pkg.*` take `repo`/`dir` for that). Recursive
   walks skip symlinks/junctions (cycle safety).
2. **No silent overwrite semantics.** `fs.write` returns `created`/`overwrote`
   flags; a write over an existing file is recorded (journal) — allowed.
3. **patch.apply exactness.** Each edit's `oldText` must match exactly
   `expectedCount ?? 1` times; 0 matches errors `PATCH_NO_MATCH` with
   `nearestCandidateLines`; > hoped errors `PATCH_AMBIGUOUS` with both
   `occurrences` and `expected`. No partial application on failure.
4. **proc.spawn is argv-typed.** No shell. `cmd` + `args` array, explicit
   `cwd`, hard `timeoutMs`, captured stdout/stderr, structured result with
   `exitCode`, `timedOut`, `error` (for ENOENT-style failures).
5. **proc.start handles.** Long-running processes get a `handleId`; output is
   buffered (bounded) and readable via `proc.readOutput`; `proc.status`
   reports `running`/`exitCode`/`outputBytes`; `proc.stop` kills it.
6. **test.run structured.** Returns `framework`, `passed`, `failed`,
   `total`, `failures: [{name, file, message}]`. Node uses its junit
   reporter; pytest uses `--junitxml`. No output-text-only results.
7. **snapshot/rollback manifest.** Snapshot captures all workspace files
   excluding `.git`, `node_modules`, `.nc-tools`; rollback restores manifest
   files and removes files created after the snapshot. Unknown id errors
   `ERR_UNKNOWN_SNAPSHOT` with available list.
8. **batch.execute.** Runs up to 25 sub-calls; each is executed and journaled
   individually; one failure does not abort the rest; nesting itself errors
   `ERR_REFUSED`.
9. **Deterministic ordering.** `fs.list` and `search.*` results are
   sorted; journal `seq` is monotonically increasing.
10. **Session env.** `env.set` overrides are inherited by every subsequent
    proc.* call; `env.get` resolves session → host → `unset`.
11. **Proc control.** `proc.list` returns the OS process table
    (`{pid, name, memKb?}`, optional name `filter`, capped by `maxResults`);
    `proc.kill` kills by PID and errors `ERR_PROC_NOT_FOUND` (ESRCH) or
    `ERR_REFUSED` (EPERM).
12. **Branch ops.** `git.branch` lists or creates; `git.checkout` switches
    (or creates); `git.push`/`git.pull` set tracking / stay ff-only by
    default. All git tools error `ERR_GIT` with the stderr tail on failure.

## 4. Journal schema (JSONL, one object per line)

tool call event:
```json
{"ts": "ISO-8601", "seq": 12, "kind": "tool.call", "tool": "fs.read",
 "args": {"path": "x.txt"}}
```

tool result event:
```json
{"ts": "ISO-8601", "seq": 13, "kind": "tool.result", "tool": "fs.read",
 "callSeq": 12, "ok": true, "result": {...},
 "error": null, "durationMs": 4}
```

On error, `ok` is `false`, `error` is `{code, message, hint?}` and
`result` is `null`. Journal `seq` is shared across call and result events.

## 5. Error taxonomy

The registry in `crates/nct-core/src/errors.rs` (`codes::ALL`) is the single
source of truth; a byte-match test enforces that this table and the registry
list exactly the same set, in both directions. Every row below starts with
exactly one code (combined rows are not parseable by the gate).

| Code | Meaning |
|---|---|
| `ERR_BAD_INPUT` | argument validation failure (hint: `missing` / `expected` field where extractable) |
| `ERR_BAD_EDIT` | edit arguments invalid before matching (e.g. empty oldText) |
| `ERR_BAD_PATH` | empty / non-path value |
| `ERR_BAD_REGEX` | invalid search pattern |
| `ERR_BINARY_FILE` | path is a binary file (fs.read refuses >30% NUL bytes; hint: readRange) |
| `ERR_CMD_NOT_FOUND` | spawned command not found (hint: binary + PATH note) |
| `ERR_EMBED_UNAVAILABLE` | local embedding model not loaded (semantic rerank/grounding) |
| `ERR_ENGINE` | text-extraction/rerank engine failure |
| `ERR_GIT` | git command failed (hint: stderr) |
| `ERR_GIT_SPAWN` | git binary could not be spawned |
| `ERR_INTERNAL` | unclassified internal defect (io/serde kinds with no specific code) |
| `ERR_IS_DIRECTORY` | used a directory where a file is required |
| `ERR_NET` | network request failure |
| `ERR_NETWORK` | package-manager network failure |
| `ERR_NOT_A_REPO` | workspace has no .git |
| `ERR_NOT_FOUND` | path or file missing (hint: `nearestExisting` files) |
| `ERR_NO_KEY` | required API key absent from env (names the `NCTOOLS_*_KEY` var) |
| `ERR_NO_TESTS` | runner discovered zero tests (hint: check the path/patterns) |
| `ERR_PANIC` | tool handler panicked; kernel caught it — server survives, error is retryable |
| `ERR_PARSE` | runner produced no structured report / response body unparseable |
| `ERR_PERMISSION` | OS denied access (file/dir permission; io PermissionDenied) |
| `ERR_PKG` | package manager operation failed (hint: captured stderr) |
| `ERR_PROC_NOT_FOUND` | no process with that pid (proc.kill) |
| `ERR_REFUSED` | explicitly refused operation (e.g. root delete, batch nesting) |
| `ERR_RENDER` | headless render attempt failed |
| `ERR_RENDER_UNAVAILABLE` | no system browser available for render escalation |
| `ERR_SPAWN` | process spawn failed for a reason other than missing command |
| `ERR_SSRF_BLOCKED` | request to private/loopback/metadata target refused (fail-closed) |
| `ERR_TEST_PARSE` | test runner output could not be parsed |
| `ERR_TIMEOUT` | operation exceeded its budget (request, spawn, or wait) |
| `ERR_UNKNOWN_HANDLE` | process handle id unknown (hint: known list) |
| `ERR_UNKNOWN_SNAPSHOT` | snapshot id unknown (hint: available) |
| `ERR_UNKNOWN_TOOL` | tool not in surface (hint: `available` list, `didYouMean` nearest name, `retryWith` canonical MCP wire name) |
| `PATCH_AMBIGUOUS` | edit matches more than expected |
| `PATCH_NO_MATCH` | edit oldText not found (hint: nearest candidate lines) |

Every error must have exactly these fields: `code`, `message`, optional
`hint` (JSON object). Codes must match byte-for-byte.

### 5.1 Self-healing reminders

Errors carry remediation, not just diagnosis:

- `ERR_UNKNOWN_TOOL` teaches the fix in one turn: when the sent name is close
  to a registered tool (Levenshtein over the normalized wire form), the hint
  names it (`didYouMean`) and a retryable MCP wire name (`retryWith`, e.g.
  `mcp__nc-tools__fs_read`); the message states the accepted wire forms.
  Case-only slips (`FS_READ`) resolve without error.
- `ERR_PANIC` proves crash safety: a panicking handler never kills the server;
  the panic message is the root cause in `error.message`, and subsequent
  calls on the same connection succeed.
- io errors surface their specific kind (`NotFound` → `ERR_NOT_FOUND`,
  `PermissionDenied` → `ERR_PERMISSION`, `TimedOut` → `ERR_TIMEOUT`,
  `ConnectionRefused` → `ERR_REFUSED`) rather than a blanket internal code.

## 6. Transport bindings

### 6.1 MCP stdio (required for conformance)

- JSON-RPC 2.0 over stdio, newline-delimited JSON (one message per line).
- Supported methods: `initialize` (protocolVersion `2024-11-05`,
  serverInfo `{name: "nc-tools", version: <semver>}`), `notifications/initialized`
  (no response), `tools/list` (each tool has `name`, `description`,
  `inputSchema` — a JSON Schema object), `tools/call`
  (`{name, arguments}` → `{content: [{type:"text", text}], isError}`).
- **Tool-name aliases**: `tools/call` accepts the dotted surface name plus
  two wire forms: single-underscore (`fs_stat`) and double-underscore
  (`fs__stat` — what agent loops emit for providers that forbid dotted
  names). Resolution order: exact → `__`→`.` → `_`→`.`; the journal records
  the canonical dotted name. No surface tool name contains `_`, so the
  rewrites are unambiguous.

### 6.2 Direct binding (any language)

A direct binding calls `kernel.call(tool, args)` and receives
`{ok, result, error, durationMs, seq}`. `error` is `null` when `ok` is true.

## 7. Conformance requirements

`tools/conformance.mjs` spawns a kernel as **an opaque process** (command +
args from env `NCTOOLS_CONFORMANCE_CMD`, cwd = a fresh temp workspace),
drives it over MCP stdio, and checks:

1. exact tool count (60) and all tool names present;
2. every tool has a JSON-Schema `inputSchema` and non-empty `description`;
3. initialization handshake shape;
4. required error codes, byte-exact, on the failure cases listed in
   `conformance/cases.mjs` (no-jail path model, patch ambiguity/miss, missing file,
   unknown tool/handle/snapshot);
5. journal pairing (equal call/result counts, `callSeq` references);
6. snapshot/rollback round-trip through the opaque protocol.

A port passes iff the same cases pass against it with only
`NCTOOLS_CONFORMANCE_CMD` changed. This is what "portable to any language"
means operationally: the suite never inspects the implementation.

## 8. Out of scope for v1

Hooks (chaos) and batch nesting depth>1 are implementation-level features;
the protocol requires only that they do not break the specified invariants.

### 3.1 Tool-surface evolution (Dr. Invi wave)

As of this wave, the surface is **81 tools** (62 → 63 → 72 → 74 → 75 → 77 → 78 → 79 → 80 → 81 with the six inventions). One additive tool:

- `sys.snapshotDiff` — diff two snapshots: what files were added, removed, or
  modified between them, plus byte totals. The review gate before a rollback.

Existing tools gained NEW arguments (all additive — strict-untyped callers
that omit them behave identically to before):

| Tool | New argument | Behavior |
|---|---|---|
| `fs.read` | — (reader rewritten to stream) | no longer loads whole file; binary detection, single-pass digest |
| `fs.write`/`writeMany` | `previousHash` in result | crash-safe atomic write via tmp+fsync+rename |
| `fs.readMany` | — (now parallel) | reads files concurrently, order preserved |
| `search.grep` | `contextBefore`, `contextAfter`, `fileType`, `fixedString` | context lines, extension filter, literal search |
| `search.semantic` | — (chunk-level) | returns `symbol`/`lineStart`/`lineEnd` per hit |
| `patch.apply` | `fuzzy` | survives whitespace drift; adds `nearestDiff` hint |
| `code.symbols` | `withBody` | returns `endLine`/`doc`/`body` spans |
| `text.diff` | `wordLevel` | word-level `wordSegments` |
| `sys.rollback` | `paths` | partial rollback (restore only selected paths) |
| `batch.execute` | `dependsOn` | parallel DAG execution |

### 3.2 Agent coordination layer

The **77-tool** surface now includes a full multi-agent coordination layer
(`agent.*`) — the capability that was missing when several agents worked the
same workspace anonymously and blind to each other. State is on-disk and
cross-process, so parallel servers sharing a workspace all see it:

- `.nc-tools/agents.jsonl` — roster: every agent identity + state
- `.nc-tools/agent-messages.jsonl` — inter-agent noticeboard
- `.nc-tools/locks.jsonl` — advisory file locks

| Tool | What it does |
|---|---|
| `agent.register` | Mint or resume an identity. `{agentId}` restores a known id (crash/compaction continuation); absent = resume this session's agent or mint `agent-<n>` (chronological). |
| `agent.list` | Chronological roster: id, name, sid, createdAt, lastSeen, status, task. Newest-first. |
| `agent.heartbeat` | Update `{status, task}` + lastSeen so the roster stays live. |
| `agent.status` | Coordination snapshot: who's here, what each is doing, active locks, whether YOU are locked out of a path, recent messages. The "look around before you start" tool. |
| `agent.post` | Write to the noticeboard. `{to}` = direct message, omit = broadcast. `kind` = note\|question\|request\|handoff\|bug\|hold\|resume. |
| `agent.messages` | Read messages (filter by to/from/kind, newest-first). |
| `agent.lock` | Advisory lock on a path; `holdMs` auto-releases even on crash (default 10 min). |
| `agent.unlock` | Release a lock you hold. Won't steal another's live lock. |
| `agent.compact` | Record a compaction checkpoint for this agent (summary + nextHint); agent.resume later follows it |
| `agent.resume` | Follow the most recent compact checkpoint: re-registers the same agentId, returns last summary + nextHint |
| `agent.peers` | List agents on OTHER workspaces from the global index (~/.nc-tools/agents.jsonl, override NCTOOLS_AGENT_HOME) |
| `agent.locks` | List active locks + expiry. Call before editing. |

**Identity + continuity (crash/compaction):**
- A client sends `agentId` in MCP `initialize`'s `clientInfo`; the kernel binds
  that id so a restart keeps the same identity + history.
- `agent.register {agentId}` resumes; a known id is honored, not renumbered.
- No `agentId`: same session (by `sid`) resumes; a new session mints the next
  chronological `agent-<n>`. Sids are now counter-unique so two servers in one
  process never collide onto one identity.

**Conflict + awareness:** agents call `agent.status` on entry, `agent.lock`
before editing a shared file, `agent.locks` before writing, and `agent.post`
to broadcast bugs/holds/handoffs — so agents that never met can still see and
coordinate through the shared workspace state.

### 3.3 Inventions wave (code.graph, fs.watch, net.research, net.contradict, sys.replay, proc.diff)

The **81-tool** surface adds six tools no normal developer invented. Each is
machine-proven (unit tests + live black-box proofs). Surface history:
75 (side-channel wave) → 77 (net.research + net.contradict) → 78 (sys.replay)
→ 79 (proc.diff) → 80 (code.graph) → 81 (fs.watch).

| Tool | What it does | Why it's novel |
|---|---|---|
| `net.research` | Multi-hop conductor: search → fetch top N → extract query-relevant span → verify → cite in ONE call. Returns answer + grounded sources + confidence. | Chains 5 primitives instead of 5 round-trips. Live: C inventor → 2/3 grounded, conf 0.667. |
| `net.contradict` | Adversarial evidence: search the claim AND its negation, classify supporting/contradicting, verdict confirmed\|contested\|unsupported\|insufficient. | Engines can't answer "why might this be wrong" — the embedder does. Live: Rust-faster-than-Go → contested (3 vs 3, real counter-evidence). |
| `sys.replay` | Replay journal entries as tool calls. dryRun=true (default) shows what would run, no side effects; dryRun=false re-executes and diffs fresh vs recorded. | The journal was write-only; now it's a reproducible instruction log. Live: re-ran an fs.write, diverged: 0. |
| `proc.diff` | Runtime process-tree diff: capture-run-capture around a command, or diff two stored snapshots. Started/stopped/changed by pid. | proc.list is a snapshot; this answers "what did this command spawn". Live: captured 409, run-node surfaced cmd/conhost with 73 changed. |
| `code.graph` | Cross-file reference graph WITHOUT a language server: who calls whom, callerFile+line+kind. `callees=X` (callers-of) / `callers=X` (calls-from) filters. | Name-resolution within scope (brace/indent spans, word-boundary refs). Live: `callees=my_agent_id` → real call sites in coordination.rs. |
| `fs.watch` | Semantic file watch: polls a file, embeds content, returns ONLY on meaning-cross-threshold (cos < 0.995) — whitespace/comment edits stay silent. | proc.watch re-runs on bytes; this watches semantics. 10× fewer rebuilds during formatting. |
