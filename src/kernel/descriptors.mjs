// Tool descriptors for MCP exposure: JSON schema per kernel tool.
export function toolDescriptors() {
  const s = (properties, required = []) => ({
    type: 'object', properties, required, additionalProperties: false,
  });
  const path = { type: 'string', description: 'Path relative to workspace root' };

  return [
    { name: 'fs.read', description: 'Read a text file with line numbers. Supports offset/limit paging. Returns a digest (hash+mtime) — use it to avoid re-reading unchanged files.', inputSchema: s({ path, offset: { type: 'integer', minimum: 1, description: '1-based line to start from' }, limit: { type: 'integer', minimum: 1 } }, ['path']) },
    { name: 'fs.readMany', description: 'Read up to 50 files in ONE call. Each item returns ok/content or a structured error. Prefer this over N separate fs.read calls.', inputSchema: s({ paths: { type: 'array', items: { type: 'string' }, minItems: 1, maxItems: 50 }, limit: { type: 'integer', minimum: 1 } }, ['paths']) },
    { name: 'fs.write', description: 'Write a file (creates parent dirs). Returns created/overwrote.', inputSchema: s({ path, content: { type: 'string' } }, ['path', 'content']) },
    { name: 'fs.writeMany', description: 'Write up to 50 files in ONE call: [{path, content}]. Per-item ok/error results.', inputSchema: s({ files: { type: 'array', minItems: 1, maxItems: 50, items: { type: 'object', properties: { path: { type: 'string' }, content: { type: 'string' } }, required: ['path', 'content'], additionalProperties: false } } }, ['files']) },
    { name: 'fs.list', description: 'List directory entries.', inputSchema: s({ path, recursive: { type: 'boolean' } }) },
    { name: 'fs.stat', description: 'Stat a path (exists, type, size).', inputSchema: s({ path }, ['path']) },
    { name: 'fs.mkdir', description: 'Create a directory.', inputSchema: s({ path, recursive: { type: 'boolean' } }, ['path']) },
    { name: 'fs.delete', description: 'Delete a file or directory (needs recursive for dirs).', inputSchema: s({ path, recursive: { type: 'boolean' } }, ['path']) },
    { name: 'fs.move', description: 'Move/rename a file or directory.', inputSchema: s({ from: path, to: path }, ['from', 'to']) },
    { name: 'patch.apply', description: 'Apply exact-match search/replace edits to a file. Each edit: {oldText, newText, expectedCount?}. Fails with structured hints (occurrence counts, nearest candidate lines) if not found or ambiguous.', inputSchema: s({ path, edits: { type: 'array', minItems: 1, items: { type: 'object', properties: { oldText: { type: 'string' }, newText: { type: 'string' }, expectedCount: { type: 'integer', minimum: 1 } }, required: ['oldText', 'newText'], additionalProperties: false } } }, ['path', 'edits']) },
    { name: 'patch.applyMany', description: 'Apply patch edits to up to 20 files in ONE call: [{path, edits:[{oldText,newText,expectedCount?}]}]. Per-file ok/error results.', inputSchema: s({ edits: { type: 'array', minItems: 1, maxItems: 20, items: { type: 'object', properties: { path: { type: 'string' }, edits: { type: 'array', minItems: 1, items: { type: 'object', properties: { oldText: { type: 'string' }, newText: { type: 'string' }, expectedCount: { type: 'integer', minimum: 1 } }, required: ['oldText', 'newText'], additionalProperties: false } } }, required: ['path', 'edits'], additionalProperties: false } } }, ['edits']) },
    { name: 'search.grep', description: 'Regex search across files. Returns file/line/text matches.', inputSchema: s({ pattern: { type: 'string' }, path, glob: { type: 'string' }, maxResults: { type: 'integer', minimum: 1, maximum: 1000 } }, ['pattern']) },
    { name: 'search.files', description: 'Find files by glob pattern (e.g. "**/*.test.mjs").', inputSchema: s({ pattern: { type: 'string' }, path }, ['pattern']) },
    { name: 'git.status', description: 'Git status: branch, head, changed files.', inputSchema: s({}) },
    { name: 'git.diff', description: 'Unified diff of unstaged changes.', inputSchema: s({ path }) },
    { name: 'git.add', description: 'Stage files.', inputSchema: s({ paths: { type: 'array', items: { type: 'string' }, minItems: 1 } }, ['paths']) },
    { name: 'git.commit', description: 'Commit staged changes.', inputSchema: s({ message: { type: 'string' } }, ['message']) },
    { name: 'git.log', description: 'Recent commits.', inputSchema: s({ maxCount: { type: 'integer', minimum: 1, maximum: 200 } }) },
    { name: 'proc.spawn', description: 'Run a program with typed argv (no shell). Captures stdout/stderr, exit code, hard timeout.', inputSchema: s({ cmd: { type: 'string' }, args: { type: 'array', items: { type: 'string' } }, cwd: { type: 'string' }, timeoutMs: { type: 'integer', minimum: 100, maximum: 600000 } }, ['cmd']) },
    { name: 'proc.start', description: 'Start a LONG-RUNNING background process (server, watcher). Returns a handleId. NOT for one-shot commands — use proc.spawn for those.', inputSchema: s({ cmd: { type: 'string' }, args: { type: 'array', items: { type: 'string' } }, cwd: { type: 'string' }, maxDurationMs: { type: 'integer', minimum: 1000, maximum: 3600000 } }, ['cmd']) },
    { name: 'proc.status', description: 'Status of a background process handle: running, exitCode, outputBytes, uptime.', inputSchema: s({ handleId: { type: 'string' } }, ['handleId']) },
    { name: 'proc.readOutput', description: 'Read recent output (stdout+stderr merged) of a background process.', inputSchema: s({ handleId: { type: 'string' }, fromEnd: { type: 'integer', minimum: 100, maximum: 100000 } }, ['handleId']) },
    { name: 'proc.stop', description: 'Stop a background process by handle.', inputSchema: s({ handleId: { type: 'string' }, force: { type: 'boolean' } }, ['handleId']) },
    { name: 'test.run', description: 'Run a test suite and get STRUCTURED results: passed/failed counts, failing test names + messages. Frameworks: node (node:test), pytest.', inputSchema: s({ framework: { type: 'string', enum: ['node', 'pytest'] }, path: { type: 'string' }, timeoutMs: { type: 'integer', minimum: 1000, maximum: 600000 } }) },
    { name: 'pkg.add', description: 'Install packages (npm or pip). Structured result; ERR_NETWORK hint if registry unreachable.', inputSchema: s({ manager: { type: 'string', enum: ['npm', 'pip'] }, names: { type: 'array', items: { type: 'string' }, minItems: 1 }, dev: { type: 'boolean' }, timeoutMs: { type: 'integer', minimum: 1000 } }, ['names']) },
    { name: 'pkg.list', description: 'List installed packages for npm (from package.json) or pip.', inputSchema: s({ manager: { type: 'string', enum: ['npm', 'pip'] } }) },
    { name: 'pkg.scripts', description: 'List npm scripts defined in package.json.', inputSchema: s({}) },
    { name: 'pkg.runScript', description: 'Run an npm script with typed args. Returns exit code + captured output.', inputSchema: s({ name: { type: 'string' }, args: { type: 'array', items: { type: 'string' } }, timeoutMs: { type: 'integer', minimum: 1000 } }, ['name']) },
    { name: 'net.http', description: 'Perform an HTTP request. Returns status, headers, body (capped), duration. Replaces curl/wget.', inputSchema: s({ method: { type: 'string', enum: ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD'] }, url: { type: 'string' }, headers: { type: 'object' }, body: { type: 'string' }, timeoutMs: { type: 'integer', minimum: 100, maximum: 120000 } }, ['url']) },
    { name: 'net.probePort', description: 'Check whether a TCP port is open. Replaces nc/netstat probing.', inputSchema: s({ port: { type: 'integer', minimum: 1, maximum: 65535 }, host: { type: 'string' }, timeoutMs: { type: 'integer', minimum: 100, maximum: 10000 } }, ['port']) },
    { name: 'env.get', description: 'Read an environment variable (session override wins over host).', inputSchema: s({ name: { type: 'string' } }, ['name']) },
    { name: 'env.set', description: 'Set a session environment variable; all subsequent proc.* calls inherit it.', inputSchema: s({ name: { type: 'string' }, value: { type: 'string' } }, ['name', 'value']) },
    { name: 'env.list', description: 'List session environment overrides.', inputSchema: s({}) },
    { name: 'sys.snapshot', description: 'Capture a workspace snapshot (files + dirs, excludes .git/node_modules/.nc-tools). Returns an id usable with sys.rollback. Use before risky edits so you can undo.', inputSchema: s({ label: { type: 'string' } }) },
    { name: 'sys.rollback', description: 'Restore the workspace to a snapshot: restores manifest files, removes files created after the snapshot.', inputSchema: s({ id: { type: 'string' } }, ['id']) },
    { name: 'sys.listSnapshots', description: 'List available snapshots.', inputSchema: s({}) },
    { name: 'batch.execute', description: 'Run up to 25 kernel tool calls in ONE round-trip: [{tool, args}]. Each sub-call is individually executed and journaled; failures do not abort the batch. Use for independent multi-step work.', inputSchema: s({ calls: { type: 'array', minItems: 1, maxItems: 25, items: { type: 'object', properties: { tool: { type: 'string' }, args: { type: 'object' } }, required: ['tool'], additionalProperties: false } } }, ['calls']) },
    { name: 'sys.journal', description: 'Read the session journal (your own tool-call trail).', inputSchema: s({ lastN: { type: 'integer', minimum: 1, maximum: 1000 } }) },
    { name: 'sys.workspace', description: 'Workspace info: root path, platform.', inputSchema: s({}) },
  ];
}
