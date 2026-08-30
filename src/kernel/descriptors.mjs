// Tool descriptors for MCP exposure: JSON schema per kernel tool.
export function toolDescriptors() {
  const s = (properties, required = []) => ({
    type: 'object', properties, required, additionalProperties: false,
  });
  const path = { type: 'string', description: 'Path relative to workspace root' };

  return [
    { name: 'fs.read', description: 'Read a text file with line numbers. Supports offset/limit paging.', inputSchema: s({ path, offset: { type: 'integer', minimum: 1, description: '1-based line to start from' }, limit: { type: 'integer', minimum: 1 } }, ['path']) },
    { name: 'fs.write', description: 'Write a file (creates parent dirs). Returns created/overwrote.', inputSchema: s({ path, content: { type: 'string' } }, ['path', 'content']) },
    { name: 'fs.list', description: 'List directory entries.', inputSchema: s({ path, recursive: { type: 'boolean' } }) },
    { name: 'fs.stat', description: 'Stat a path (exists, type, size).', inputSchema: s({ path }, ['path']) },
    { name: 'fs.mkdir', description: 'Create a directory.', inputSchema: s({ path, recursive: { type: 'boolean' } }, ['path']) },
    { name: 'fs.delete', description: 'Delete a file or directory (needs recursive for dirs).', inputSchema: s({ path, recursive: { type: 'boolean' } }, ['path']) },
    { name: 'fs.move', description: 'Move/rename a file or directory.', inputSchema: s({ from: path, to: path }, ['from', 'to']) },
    { name: 'patch.apply', description: 'Apply exact-match search/replace edits to a file. Each edit: {oldText, newText, expectedCount?}. Fails with structured hints if not found or ambiguous.', inputSchema: s({ path, edits: { type: 'array', minItems: 1, items: { type: 'object', properties: { oldText: { type: 'string' }, newText: { type: 'string' }, expectedCount: { type: 'integer', minimum: 1 } }, required: ['oldText', 'newText'], additionalProperties: false } } }, ['path', 'edits']) },
    { name: 'search.grep', description: 'Regex search across files. Returns file/line/text matches.', inputSchema: s({ pattern: { type: 'string' }, path, glob: { type: 'string' }, maxResults: { type: 'integer', minimum: 1, maximum: 1000 } }, ['pattern']) },
    { name: 'search.files', description: 'Find files by glob pattern (e.g. "**/*.test.mjs").', inputSchema: s({ pattern: { type: 'string' }, path }, ['pattern']) },
    { name: 'git.status', description: 'Git status: branch, head, changed files.', inputSchema: s({}) },
    { name: 'git.diff', description: 'Unified diff of unstaged changes.', inputSchema: s({ path }) },
    { name: 'git.add', description: 'Stage files.', inputSchema: s({ paths: { type: 'array', items: { type: 'string' }, minItems: 1 } }, ['paths']) },
    { name: 'git.commit', description: 'Commit staged changes.', inputSchema: s({ message: { type: 'string' } }, ['message']) },
    { name: 'git.log', description: 'Recent commits.', inputSchema: s({ maxCount: { type: 'integer', minimum: 1, maximum: 200 } }) },
    { name: 'proc.spawn', description: 'Run a program with typed argv (no shell). Captures stdout/stderr, exit code, hard timeout.', inputSchema: s({ cmd: { type: 'string' }, args: { type: 'array', items: { type: 'string' } }, cwd: { type: 'string' }, timeoutMs: { type: 'integer', minimum: 100, maximum: 600000 } }, ['cmd']) },
    { name: 'sys.journal', description: 'Read the session journal (your own tool-call trail).', inputSchema: s({ lastN: { type: 'integer', minimum: 1, maximum: 1000 } }) },
    { name: 'sys.workspace', description: 'Workspace info: root path, platform.', inputSchema: s({}) },
  ];
}
