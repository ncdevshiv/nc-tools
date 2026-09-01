// Wave-7 tests: parallel agents (cross-process concurrency on one workspace)
// and MCP idle auto-sleep. Both are real multi-process tests.
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { Kernel } from '../oracle/kernel/kernel.mjs';
import { SERVER_BIN } from './driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const serverPath = SERVER_BIN;

/** Spawn a kernel as a child process and give it an rpc() helper. */
function spawnKernel(root) {
  const child = spawn(serverPath, [root], { stdio: ['pipe', 'pipe', 'pipe'] });
  child.stderr.on('data', () => {});
  let id = 0;
  const pending = new Map();
  child.stdout.on('data', (buf) => {
    for (const line of buf.toString('utf8').split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let msg;
      try { msg = JSON.parse(t); } catch { continue; }
      const p = pending.get(msg.id);
      if (p) { pending.delete(msg.id); p(msg); }
    }
  });
  const rpc = (method, params) => new Promise((res, rej) => {
    const myId = ++id;
    const timer = setTimeout(() => rej(new Error(`${method} timeout`)), 20_000);
    pending.set(myId, (m) => { clearTimeout(timer); res(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: myId, method, params }) + '\n');
  });
  const close = () => { try { child.stdin.end(); } catch {}; };
  return { child, rpc, close };
}

test('two parallel kernel processes share one workspace and ONE journal without torn lines', async () => {
  const root = mkdtempSync(join(tmpdir(), 'nc-parallel-'));
  const a = spawnKernel(root);
  const b = spawnKernel(root);
  try {
    await a.rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '0' } });
    await b.rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '0' } });

    // both agents work concurrently on the same workspace: interleaved writes
    const jobs = [];
    for (let i = 0; i < 10; i++) {
      jobs.push(a.rpc('tools/call', { name: 'fs.write', arguments: { path: `agent-a-${i}.txt`, content: 'a'.repeat(2000) } }));
      jobs.push(b.rpc('tools/call', { name: 'fs.write', arguments: { path: `agent-b-${i}.txt`, content: 'b'.repeat(2000) } }));
      jobs.push(a.rpc('tools/call', { name: 'fs.stat', arguments: { path: `agent-b-${i}.txt` } }));
      jobs.push(b.rpc('tools/call', { name: 'fs.read', arguments: { path: `agent-a-${i}.txt` } }));
    }
    // IMPORTANT: with two agents on one workspace, a cross-agent read may
    // legitimately race a write (ERR_NOT_FOUND is the correct structured
    // outcome, not an error). The true parallel-safety invariants are:
    // (1) no process crashes, (2) every journal line parses (no torn lines),
    // (3) any error is a structured kernel error, never an EIO/corruption.
    const results = await Promise.all(jobs);
    for (const r of results) {
      if (r.result?.isError) {
        const text = JSON.parse(r.result.content[0].text);
        assert.ok(text.error?.code, 'kernel errors must be structured');
      }
    }

    // EVERY line of the journal must be valid JSON (no torn/interleaved lines)
    const journalPath = join(root, '.nc-tools', 'journal.jsonl');
    const lines = readFileSync(journalPath, 'utf8').split('\n').filter(Boolean);
    assert.ok(lines.length >= 80, `expected many journal lines, got ${lines.length}`);
    for (const line of lines) {
      const ev = JSON.parse(line); // throws if torn
      assert.ok(ev.seq > 0);
      assert.ok(ev.kind === 'tool.call' || ev.kind === 'tool.result');
    }

    // call/result pairing holds globally: every result's callSeq references a call
    const calls = [], resultsArr = [];
    for (const line of lines) {
      const ev = JSON.parse(line);
      if (ev.kind === 'tool.call') calls.push(ev);
      else resultsArr.push(ev);
    }
    const callSeqs = new Set(calls.map((c) => c.seq));
    for (const r of resultsArr) assert.ok(callSeqs.has(r.callSeq), `orphaned result callSeq=${r.callSeq}`);
  } finally {
    a.close(); b.close();
    await new Promise((r) => setTimeout(r, 300));
    rmSync(root, { recursive: true, force: true });
  }
});

test('MCP server auto-sleeps after idle timeout and exits', async () => {
  const root = mkdtempSync(join(tmpdir(), 'nc-idle-'));
  const child = spawn(serverPath, [root], {
    stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, NCTOOLS_MCP_IDLE_MS: '1500' },
  });
  child.stderr.on('data', () => {});
  // serve one call, then wait — the process should exit by itself
  let id = 0;
  const pending = new Map();
  child.stdout.on('data', (buf) => {
    for (const line of buf.toString('utf8').split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let msg;
      try { msg = JSON.parse(t); } catch { continue; }
      const p = pending.get(msg.id);
      if (p) { pending.delete(msg.id); p(msg); }
    }
  });
  // rpc with a fail-fast timeout so a hung server fails the test instead of
  // stalling the whole suite.
  const rpc = (method, params) => new Promise((res, rej) => {
    const myId = ++id;
    const timer = setTimeout(() => rej(new Error(`${method} timeout`)), 20_000);
    pending.set(myId, (m) => { clearTimeout(timer); res(m); });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: myId, method, params }) + '\n');
  });
  await rpc('initialize', { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 't', version: '0' } });
  await rpc('tools/call', { name: 'sys.workspace', arguments: {} });

  // Generous budget: the Rust binary needs ~4s cold start (vs ~1.5s for the
  // archived JS oracle) before the 1.5s idle window even begins, so a fixed
  // 8s wait flaked under load. The assertion is "exits by itself", not speed.
  const exited = await new Promise((res) => {
    const timer = setTimeout(() => res(false), 20_000);
    child.on('exit', (code) => { clearTimeout(timer); res(code === 0); });
  });
  assert.equal(exited, true, 'server should exit after idle timeout');
  rmSync(root, { recursive: true, force: true });
});
