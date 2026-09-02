// Conformance runner: spawns ANY nc-tools kernel implementation as an opaque
// MCP-stdio process and verifies the protocol contract in docs/PROTOCOL.md.
//
// Usage: NCTOOLS_CONFORMANCE_CMD="node src/mcp/server.mjs" node tools/conformance.mjs
//   For a port (Rust/Go/Python), point the same env var at that binary and
//   re-run — the cases are identical and language-neutral.
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { conformanceCases } from '../conformance/cases.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const defaultBin = process.platform === 'win32' ? 'nc-tools-mcp.exe' : 'nc-tools-mcp';
const defaultCmd = join(here, '..', 'target', 'release', defaultBin);
const cmdStr = process.env.NCTOOLS_CONFORMANCE_CMD || defaultCmd;
// split command string (support simple `cmd args` shape)
const [cmd, ...cmdArgs] = cmdStr.split(' ').filter(Boolean);

function runCase(wsRoot, testCase) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(cmd, [...cmdArgs, wsRoot], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    let buf = '';
    let id = 0;
    const pending = new Map();
    const saved = {};
    child.stdout.on('data', (d) => {
      buf += d.toString('utf8');
      let idx;
      while ((idx = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, idx).trim();
        buf = buf.slice(idx + 1);
        if (!line) continue;
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        const p = pending.get(msg.id);
        if (p) { pending.delete(msg.id); p(msg); }
      }
    });
    const send = (method, params) => new Promise((res) => {
      const myId = ++id;
      pending.set(myId, res);
      child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: myId, method, params }) + '\n');
    });
    const timeout = setTimeout(() => {
      child.kill();
      rejectPromise(new Error(`${testCase.name}: timeout`));
    }, 120_000);

    (async () => {
      try {
        await send('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'conformance', version: '1' } });
        for (const step of testCase.steps) {
          if (step.type === 'mcp') {
            let params = step.params;
            let expected = step.expect;
            // substitute saved values like '#snapId#'
            if (typeof params?.arguments === 'object') {
              const argsPlus = JSON.parse(JSON.stringify(params.arguments).replace(/"#([a-zA-Z]+)#"/g, (m, name) => JSON.stringify(saved[name] ?? m)));
              params = { ...params, arguments: argsPlus };
            }
            const resp = await send(step.method, params);
            if (resp.error) throw new Error(`${testCase.name}: RPC error ${JSON.stringify(resp.error)}`);
            if (!expected(resp)) throw new Error(`${testCase.name}: assertion failed on ${step.method} ${step.params?.name ?? ''}`);
            // save matched JSON fields for later steps
            const text = resp.result?.content?.[0]?.text;
            if (text && step.save) {
              try { saved[step.save] = JSON.parse(text).id ?? JSON.parse(text).handleId; } catch { /* ignore */ }
            }
            if (step.method === 'tools/list' && step.save) {
              saved[step.save] = resp.result.tools.length;
            }
          }
        }
        child.kill();
        clearTimeout(timeout);
        resolvePromise({ pass: true, steps: testCase.steps.length });
      } catch (e) {
        child.kill();
        clearTimeout(timeout);
        rejectPromise(e);
      }
    })();
  });
}

(async () => {
  const allPass = [];
  const ws = mkdtempSync(join(tmpdir(), 'nc-conform-'));
  try {
    for (const c of conformanceCases) {
      try {
        const r = await runCase(ws, c);
        allPass.push({ case: c.name, pass: true, steps: r.steps });
        console.log(`  [x] ${c.name} — ${r.steps} steps`);
      } catch (e) {
        allPass.push({ case: c.name, pass: false, error: e.message });
        console.log(`  [ ] ${c.name} — FAIL: ${e.message}`);
      }
    }
  } finally {
    rmSync(ws, { recursive: true, force: true });
  }
  const passed = allPass.filter((p) => p.pass).length;
  console.log(`\nCONFORMANCE: ${passed}/${allPass.length} passed against "${cmdStr}"`);
  if (passed !== allPass.length) process.exit(1);
})();
