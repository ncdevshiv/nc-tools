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
  caseTemplate('tool surface: exactly 48 tools with schemas', [
    { type: 'mcp', method: 'tools/list', params: {},
      expect: (res) => {
        const tools = res.result.tools;
        if (tools.length !== 48) throw new Error(`expected 48 tools, got ${tools.length}`);
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
];
