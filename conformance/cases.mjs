// Conformance cases for the nc-tools protocol (docs/PROTOCOL.md).
// These are black-box: they drive a kernel via MCP stdio and assert exact
// observable behavior — tool count, error codes, journal invariants.
// Any reimplementation (Rust/Go/Python/...) must pass the SAME cases.

const caseTemplate = (name, steps) => ({
  name,
  // Each step: {type: 'mcp', method, params, expect} or
  //           {type: 'fs', ...} for workspace preparation outside the protocol
  steps,
});

export const conformanceCases = [
  caseTemplate('initialize handshake', [
    { type: 'mcp', method: 'initialize', params: { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'conformance', version: '1' } },
      expect: (res) => res.result.serverInfo.name === 'nc-tools' && res.result.protocolVersion === '2024-11-05' },
  ]),
  caseTemplate('tool surface: exactly 57 tools with schemas', [
    { type: 'mcp', method: 'tools/list', params: {},
      expect: (res) => {
        const tools = res.result.tools;
        if (tools.length !== 57) throw new Error(`expected 57 tools, got ${tools.length}`);
        for (const t of tools) {
          if (!t.inputSchema || t.inputSchema.type !== 'object') throw new Error(`${t.name} missing object inputSchema`);
          if (!t.description) throw new Error(`${t.name} missing description`);
        }
        return true;
      } },
  ]),
  caseTemplate('fs.write then fs.read round-trips', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'roundtrip.txt', content: 'hello protocol\n' } },
      expect: (r) => !r.result.isError && /created/.test(r.result.content[0].text) },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 'roundtrip.txt' } },
      expect: (r) => !r.result.isError && r.result.content[0].text.includes('hello protocol') },
  ]),
  caseTemplate('ERR_NOT_FOUND with nearestExisting hint', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 'no-such-file.txt' } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'ERR_NOT_FOUND' && Array.isArray(err.error.hint.nearestExisting);
      } },
  ]),
  caseTemplate('paths outside the base dir are writable (no jail)', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: '../escape.txt', content: 'x' } },
      expect: (r) => !r.result.isError && r.result.content[0].text.includes('created') },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.delete', arguments: { path: '../escape.txt' } },
      expect: (r) => !r.result.isError },
  ]),
  caseTemplate('patch.apply exact match + PATCH_NO_MATCH hints', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'app.js', content: 'function add(a, b) {\n  return a + b;\n}\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'patch.apply', arguments: { path: 'app.js', edits: [{ oldText: 'return a + b;', newText: 'return a + b + 0;' }] } },
      expect: (r) => !r.result.isError && r.result.content[0].text.includes('replacements') },
    { type: 'mcp', method: 'tools/call', params: { name: 'patch.apply', arguments: { path: 'app.js', edits: [{ oldText: 'return a * b;', newText: 'x' }] } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'PATCH_NO_MATCH' && Array.isArray(err.error.hint.nearestCandidateLines);
      } },
  ]),
  caseTemplate('patch.apply ambiguity guard', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'dup.txt', content: 'same\nother\nsame\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'patch.apply', arguments: { path: 'dup.txt', edits: [{ oldText: 'same', newText: 'new' }] } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'PATCH_AMBIGUOUS' && err.error.hint.occurrences === 2;
      } },
  ]),
  caseTemplate('search.grep returns file/line/text', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'g.js', content: 'const needle = 42;\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'search.grep', arguments: { pattern: 'needle' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.matches.length === 1 && res.matches[0].file === 'g.js' && res.matches[0].line === 1;
      } },
  ]),
  caseTemplate('ERR_UNKNOWN_TOOL with available list', [
    { type: 'mcp', method: 'tools/call', params: { name: 'bash.run', arguments: { script: 'rm -rf /' } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'ERR_UNKNOWN_TOOL' && Array.isArray(err.error.hint.available);
      } },
  ]),
  caseTemplate('proc.spawn typed argv, exit code, no shell injection', [
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'node', args: ['-e', 'console.log("spawned-ok")'] } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.exitCode === 0 && res.stdout.includes('spawned-ok');
      } },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'definitely-not-a-cmd-xyz', args: ['echo hi > pwned.txt'] } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.error?.code === 'ERR_CMD_NOT_FOUND';
      } },
  ]),
  caseTemplate('batch.execute runs mixed calls with per-item results', [
    { type: 'mcp', method: 'tools/call', params: { name: 'batch.execute', arguments: { calls: [
      { tool: 'sys.workspace', args: {} },
      { tool: 'fs.read', args: { path: 'ghost.txt' } },
    ] } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.ok === 1 && res.failed === 1;
      } },
  ]),
  caseTemplate('snapshot/rollback round-trip across the protocol', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 's.txt', content: 'original' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'sys.snapshot', arguments: { label: 'pre' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return typeof res.id === 'string' && res.files >= 1;
      }, save: 'snapId' },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 's.txt', content: 'changed' } },
      expect: (r) => !r.result.isError && r.result.content[0].text.includes('overwrote') },
    { type: 'mcp', method: 'tools/call', params: { name: 'sys.rollback', arguments: { id: '#snapId#' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 's.txt' } },
      expect: (r) => r.result.content[0].text.includes('original') },
  ]),
  caseTemplate('journal invariants: call/result pairing with callSeq', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'j.txt', content: 'x' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'sys.journal', arguments: { lastN: 100 } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        // sys.journal's own result lands AFTER its call event, so the observable
        // window is: N tool.calls (including sys.journal's) and N-1 tool.results.
        const calls = res.events.filter((e) => e.kind === 'tool.call');
        const results = res.events.filter((e) => e.kind === 'tool.result');
        if (calls.length !== results.length + 1) throw new Error(`expected results+1==calls, got calls=${calls.length} results=${results.length}`);
        const callSeqs = new Set(calls.map((c) => c.seq));
        for (const r2 of results) {
          if (!callSeqs.has(r2.callSeq)) throw new Error(`result callSeq ${r2.callSeq} orphaned`);
        }
        return true;
      } },
  ]),
  caseTemplate('env.set propagates to spawn', [
    { type: 'mcp', method: 'tools/call', params: { name: 'env.set', arguments: { name: 'CONF_TEST', value: 'visible' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'node', args: ['-e', 'console.log(process.env.CONF_TEST)'] } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.stdout.includes('visible');
      } },
  ]),
  caseTemplate('fs.copy/fs.move lifecycle across the protocol', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'orig.txt', content: 'content-A' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.copy', arguments: { from: 'orig.txt', to: 'copy.txt' } },
      expect: (r) => JSON.parse(r.result.content[0].text).copied === true },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 'copy.txt' } },
      expect: (r) => r.result.content[0].text.includes('content-A') },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.move', arguments: { from: 'copy.txt', to: 'moved.txt' } },
      expect: (r) => JSON.parse(r.result.content[0].text).moved === true },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 'moved.txt' } },
      expect: (r) => r.result.content[0].text.includes('content-A') },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.read', arguments: { path: 'copy.txt' } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'ERR_NOT_FOUND';
      } },
  ]),
  caseTemplate('git round-trip: init via proc.spawn, add, commit, status', [
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['init'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['config', 'user.email', 'conform@nc-tools.local'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['config', 'user.name', 'conformance'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'repo-file.txt', content: 'tracked content' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.add', arguments: { paths: ['repo-file.txt'] } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return Array.isArray(res.added) && res.added.includes('repo-file.txt');
      } },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.commit', arguments: { message: 'conformance commit' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return typeof res.sha === 'string' && res.sha.length >= 7;
      } },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.status', arguments: {} },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return typeof res.branch === 'string' && typeof res.head === 'string' && res.head.length >= 7;
      } },
  ]),
  caseTemplate('tool-name wire aliases: underscored forms resolve', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'alias.txt', content: 'alias body' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs_stat', arguments: { path: 'alias.txt' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return !res.error && typeof res.size === 'number' && res.size > 0;
      } },
    { type: 'mcp', method: 'tools/call', params: { name: 'sys__workspace', arguments: {} },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return !res.error && typeof res.root === 'string' && res.root.length > 0;
      } },
  ]),
  caseTemplate('fs.readRange windowed byte read', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'rr.txt', content: '0123456789\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.readRange', arguments: { path: 'rr.txt', byteOffset: 2, maxBytes: 4 } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.content === '2345' && res.byteOffset === 2 && res.byteLength === 4;
      } },
  ]),
  caseTemplate('fs.tree returns structured entries', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.mkdir', arguments: { path: 'td/sub' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'td/a.txt', content: 'x' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.tree', arguments: { path: 'td' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return Array.isArray(res.entries) && res.entries.some((e) => e.path === 'a.txt');
      } },
  ]),
  caseTemplate('code.symbols extracts named symbols', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'c.ts', content: 'class Foo {}\nfunction bar() {}\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'code.symbols', arguments: { path: 'c.ts' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return Array.isArray(res.symbols) && res.symbols.some((s) => s.name === 'Foo') && res.symbols.some((s) => s.name === 'bar');
      } },
  ]),
  caseTemplate('text.diff produces unified hunk', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'd.old.txt', content: 'a\nb\nc\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'd.new.txt', content: 'a\nCHANGED\nc\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'text.diff', arguments: { path: 'd.old.txt', path2: 'd.new.txt' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.diff.includes('@@') && /-b/.test(res.diff) && /\+CHANGED/.test(res.diff);
      } },
  ]),
  caseTemplate('search.replace dry-run reports matches only', [
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 's.js', content: 'const needle = 1;\nconst needle = 2;\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'search.replace', arguments: { pattern: 'needle', replacement: 'pin', path: 's.js' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.dryRun === true && res.totalMatches === 2 && res.files[0].changed === true;
      } },
  ]),
  caseTemplate('proc.runScript executes inline source', [
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.runScript', arguments: { language: 'js', source: 'console.log("runscript-ok")' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return res.exitCode === 0 && res.stdout.includes('runscript-ok');
      } },
  ]),
  caseTemplate('git.blame annotates every line', [
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['init'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['config', 'user.email', 'conform@nc-tools.local'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'proc.spawn', arguments: { cmd: 'git', args: ['config', 'user.name', 'conformance'] } },
      expect: (r) => JSON.parse(r.result.content[0].text).exitCode === 0 },
    { type: 'mcp', method: 'tools/call', params: { name: 'fs.write', arguments: { path: 'blame.txt', content: 'one\ntwo\n' } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.add', arguments: { paths: ['blame.txt'] } },
      expect: (r) => !r.result.isError },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.commit', arguments: { message: 'blame commit' } },
      expect: (r) => !r.result.isError && typeof JSON.parse(r.result.content[0].text).sha === 'string' },
    { type: 'mcp', method: 'tools/call', params: { name: 'git.blame', arguments: { path: 'blame.txt' } },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return Array.isArray(res.lines) && res.lines.length === 2 && res.lines[0].commit;
      } },
  ]),
  caseTemplate('sys.doctor reports kernel inventory', [
    { type: 'mcp', method: 'tools/call', params: { name: 'sys.doctor', arguments: {} },
      expect: (r) => {
        const res = JSON.parse(r.result.content[0].text);
        return typeof res.tools?.count === 'number' && res.tools.count === 57;
      } },
  ]),
  caseTemplate('search.semantic rejects empty query before model load', [
    { type: 'mcp', method: 'tools/call', params: { name: 'search.semantic', arguments: { query: '   ' } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error.code === 'ERR_BAD_INPUT' && /query/.test(err.error.message);
      } },
  ]),
  caseTemplate('search.semantic validates path and topK before ranking', [
    { type: 'mcp', method: 'tools/call', params: { name: 'search.semantic', arguments: { query: 'auth handling', path: 'conform-no-such-dir' } },
      expect: (r) => {
        // a missing dir is a clean structured error, never an embedding crash
        const err = JSON.parse(r.result.content[0].text);
        return err.error && typeof err.error.code === 'string';
      } },
    { type: 'mcp', method: 'tools/call', params: { name: 'search.semantic', arguments: { query: 'auth handling', topK: 0 } },
      expect: (r) => {
        const err = JSON.parse(r.result.content[0].text);
        return err.error && typeof err.error.code === 'string';
      } },
  ]),
];
