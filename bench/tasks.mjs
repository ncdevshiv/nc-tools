// Benchmark tasks. Each: id, category, instruction, setup (files written to a
// fresh workspace), verify (async (root, kernel) => {pass, evidence}).
// Verifiers use the real filesystem — no self-reporting.

// ============ Tier 2: discriminating tasks (multi-file, trap, TDD) ============
// These are harder on purpose: they test recovery, multi-file coordination,
// and instruction following — the dimensions where typed tools are expected
// to differentiate. Verifiers stay independent and behavioral.

const tier2 = [
  {
    id: 'multi-file-refactor',
    category: 'refactor',
    difficulty: 'hard',
    language: 'js',
    instruction: `This project has a bug-prone duplicated helper: the function parseDuration appears in multiple files under src/ with slightly different implementations. Consolidate it: create src/duration.js exporting function parseDuration(text) that handles BOTH supported forms — plain integers ("120" meaning seconds) and suffixed forms ("90s", "5m", "2h" — seconds/minutes/hours). Update every file under src/ that currently defines its own parseDuration to import the shared one from './duration.js' instead (relative imports must be correct per file depth). Every existing test in test/ must still pass when you run: node --test test/`,
    setup: () => ({
      'src/audio.js': `export function parseDuration(text) {\n  return Number(text);\n}\n\nexport function clipLength(t) {\n  return parseDuration(t) * 1000;\n}\n`,
      'src/scheduler.js': `export function parseDuration(text) {\n  const m = text.match(/^(\\d+)([smh]?)$/);\n  if (!m) return NaN;\n  const n = Number(m[1]);\n  return m[2] === 'm' ? n * 60 : m[2] === 'h' ? n * 3600 : n;\n}\n\nexport function scheduleAt(text, start) {\n  return start + parseDuration(text) * 1000;\n}\n`,
      'test/duration.test.mjs': `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { parseDuration } from '../src/duration.js';\nimport { clipLength } from '../src/audio.js';\nimport { scheduleAt } from '../src/scheduler.js';\n\ntest('plain seconds via shared module', () => {\n  assert.equal(parseDuration('120'), 120);\n  assert.equal(clipLength('2'), 2000);\n});\n\ntest('suffixed forms via shared module', () => {\n  assert.equal(scheduleAt('90s', 0), 90000);\n  assert.equal(scheduleAt('5m', 0), 300000);\n  assert.equal(scheduleAt('2h', 0), 7200000);\n});\n`,
    }),
    verify: async (root, kernel) => {
      // 1. only the canonical definition in src/duration.js may exist; every
      //    OTHER file under src/ must no longer define parseDuration
      const defs = await kernel.call('search.grep', { pattern: 'function parseDuration', path: 'src' });
      const dupes = defs.result.matches.filter((m) => m.file !== 'src/duration.js').length;
      // 2. duration.js exists and handles both forms
      const run = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--test', 'test/*.test.mjs'], cwd: '.', timeoutMs: 60_000,
      });
      const dur = await kernel.call('fs.stat', { path: 'src/duration.js' });
      const pass = dupes === 0 && dur.result?.exists && run.ok && run.result.exitCode === 0;
      return { pass, evidence: `non-canonical parseDuration defs left in src/: ${dupes}; duration.js exists: ${!!dur.result?.exists}; tests: ${(run.result?.stdout || run.result?.stderr || '').slice(-200)}` };
    },
  },
  {
    id: 'fix-trap',
    category: 'investigate',
    difficulty: 'medium',
    language: 'js',
    instruction: `Running "node src/server.js" fails with "EADDRINUSE: listen port 3000". The obvious conclusion is that another process holds port 3000 — but no other process is running. Diagnose the REAL cause in the code and fix it, without changing what the program is supposed to do (print "listening on 3000" after starting its internal listener).`,
    setup: () => ({
      'src/server.js': `// minimal stand-in for a server: a "listener" registry that refuses double binds\nconst boundPorts = new Set();\n\nfunction createListener(port) {\n  if (boundPorts.has(port)) {\n    const err = new Error('listen EADDRINUSE: address already in use ::: ' + port);\n    err.code = 'EADDRINUSE';\n    throw err;\n  }\n  boundPorts.add(port);\n  return {\n    close() { boundPorts.delete(port); },\n    get port() { return port; },\n  };\n}\n\nexport function start() {\n  // BUG: the "warmup" listener is created for a config pre-check and then\n  // abandoned without close(), so the real bind below hits EADDRINUSE.\n  const warmup = createListener(3000);\n  if (!warmup) throw new Error('warmup failed');\n  const main = createListener(3000);\n  return main;\n}\n\nif (process.argv[1] && process.argv[1].endsWith('server.js')) {\n  try {\n    const l = start();\n    console.log('listening on ' + l.port);\n  } catch (e) {\n    if (e.code === 'EADDRINUSE') {\n      console.error('EADDRINUSE: listen port 3000');\n      process.exit(1);\n    }\n    throw e;\n  }\n}\n`,
    }),
    verify: async (root, kernel) => {
      const run = await kernel.call('proc.spawn', { cmd: 'node', args: ['src/server.js'], cwd: '.', timeoutMs: 30_000 });
      const out = (run.result?.stdout || '').trim();
      const pass = run.ok && run.result.exitCode === 0 && out === 'listening on 3000';
      return { pass, evidence: `exit=${run.result?.exitCode} stdout=${JSON.stringify(out)} stderr=${(run.result?.stderr || '').slice(0, 150)}` };
    },
  },
  {
    id: 'tdd-implement',
    category: 'create',
    difficulty: 'hard',
    language: 'js',
    instruction: `Write a test file test/roman.test.mjs (node:test + node:assert/strict) with REAL failing tests first for a roman numeral converter, then create src/roman.js exporting toRoman(n) and fromRoman(s) such that: toRoman(9) === 'IX', toRoman(2024) === 'MMXXIV', toRoman(0) === '' (empty string for 0), fromRoman('XIV') === 14, fromRoman('MMXXIV') === 2024, and fromRoman(toRoman(n)) === n for 0 <= n <= 3000. Run node --test test/roman.test.mjs and make it pass. Round-trip must actually be tested in the test file for at least 5 values.`,
    setup: () => ({}),
    verify: async (root, kernel) => {
      const run = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--test', 'test/roman.test.mjs'], cwd: '.', timeoutMs: 60_000,
      });
      const testSrc = await kernel.call('fs.read', { path: 'test/roman.test.mjs' }).catch(() => null);
      const src = testSrc?.result?.content || '';
      const hasRoundTrip = /fromRoman\s*\(\s*toRoman/.test(src);
      const impl = await kernel.call('fs.stat', { path: 'src/roman.js' });
      // independent behavior probe (not trusting the agent's tests alone)
      const probe = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--input-type=module', '-e',
          `import { toRoman, fromRoman } from './src/roman.js';\nconst checks = [[9,'IX'],[2024,'MMXXIV'],[0,'']];\nfor (const [n, r] of checks) if (toRoman(n) !== r) { console.error('toRoman(' + n + ')=' + toRoman(n)); process.exit(1); }\nif (fromRoman('XIV') !== 14) { console.error('fromRoman XIV=' + fromRoman('XIV')); process.exit(1); }\nif (fromRoman('MMXXIV') !== 2024) process.exit(1);\nfor (const n of [0, 1, 42, 999, 3000]) if (fromRoman(toRoman(n)) !== n) { console.error('roundtrip ' + n); process.exit(1); }\nconsole.log('PROBE-PASS');`],
        cwd: '.', timeoutMs: 30_000,
      });
      const pass = run.ok && run.result.exitCode === 0 && hasRoundTrip && impl.result?.exists && probe.result?.exitCode === 0;
      return { pass, evidence: `agent tests exit=${run.result?.exitCode}; roundtrip-in-tests=${hasRoundTrip}; independent probe: ${probe.result?.stdout || probe.result?.stderr || probe.error?.message || 'n/a'}` };
    },
  },
];

// Tier 1 tasks (single-file, short-horizon)
const tier1 = [
  {
    id: 'fix-off-by-one',
    category: 'fix',
    difficulty: 'easy',
    language: 'js',
    instruction: `The file src/range.js has a function lastN(arr, n) that should return the LAST n elements of arr, but it returns the wrong slice. Fix it so lastN([1,2,3,4,5], 2) returns [4,5]. Do not change the function signature.`,
    setup: (root) => ({
      'src/range.js': `export function lastN(arr, n) {\n  return arr.slice(0, n);\n}\n`,
    }),
    verify: async (root, kernel) => {
      const r = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--input-type=module', '-e',
          `import { lastN } from './src/range.js';\nconst a = lastN([1,2,3,4,5], 2);\nconst b = lastN([9], 3);\nif (JSON.stringify(a) !== '[4,5]') { console.error('got', a); process.exit(1); }\nif (JSON.stringify(b) !== '[9]') { console.error('got', b); process.exit(1); }\nconsole.log('PASS');`],
        cwd: '.', timeoutMs: 30_000,
      });
      return { pass: r.ok && r.result.exitCode === 0, evidence: r.result?.stdout || r.result?.stderr || r.error?.message };
    },
  },
  {
    id: 'rename-function',
    category: 'refactor',
    difficulty: 'easy',
    language: 'js',
    instruction: `In src/calc.js, rename the function computeTotal to calculateTotal everywhere it appears (definition and all call sites). The behavior must not change. There may be multiple call sites.`,
    setup: (root) => ({
      'src/calc.js': `export function computeTotal(items) {\n  return items.reduce((s, i) => s + i.price, 0);\n}\n\nexport function withTax(items) {\n  return computeTotal(items) * 1.2;\n}\n`,
      'src/report.js': `import { computeTotal } from './calc.js';\n\nexport function headline(items) {\n  return 'Total: ' + computeTotal(items);\n}\n`,
    }),
    verify: async (root, kernel) => {
      const grep = await kernel.call('search.grep', { pattern: 'computeTotal', path: '.' });
      const stillThere = grep.result.total > 0;
      const g2 = await kernel.call('search.grep', { pattern: 'calculateTotal', path: '.' });
      const files = new Set(g2.result.matches.map((m) => m.file));
      const readCalc = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--input-type=module', '-e',
          `import { calculateTotal, withTax } from './src/calc.js';\nimport { headline } from './src/report.js';\nconst items = [{price: 10}, {price: 5}];\nif (calculateTotal(items) !== 15) process.exit(1);\nif (withTax(items) !== 18) process.exit(1);\nif (headline(items) !== 'Total: 15') process.exit(1);\nconsole.log('PASS');`],
        cwd: '.', timeoutMs: 30_000,
      });
      const pass = !stillThere && files.size === 2 && readCalc.ok && readCalc.result.exitCode === 0;
      return { pass, evidence: `computeTotal remaining: ${grep.result.total}; calculateTotal in files: ${[...files].join(',')}; behavior check: ${readCalc.result?.stdout || readCalc.result?.stderr}` };
    },
  },
  {
    id: 'implement-fn-from-spec',
    category: 'create',
    difficulty: 'medium',
    language: 'js',
    instruction: `Create a file src/utils/debounce.js exporting a default function debounce(fn, waitMs) that returns a debounced wrapper: calls within waitMs of each other collapse so only the last one executes after the silence period. Also create src/utils/debounce.test.mjs with at least 2 real test cases using node:test and node:assert, then run the test file with node --test and make sure it passes.`,
    setup: () => ({}),
    verify: async (root, kernel) => {
      const run = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--test', 'src/utils/debounce.test.mjs'],
        cwd: '.', timeoutMs: 60_000,
      });
      const srcExists = await kernel.call('fs.stat', { path: 'src/utils/debounce.js' });
      const pass = run.ok && run.result.exitCode === 0 && srcExists.result?.exists;
      return { pass, evidence: (run.result?.stdout || '') + (run.result?.stderr || '') };
    },
  },
  {
    id: 'find-and-fix-bug',
    category: 'investigate',
    difficulty: 'medium',
    language: 'js',
    instruction: `The program src/app.js crashes when run with "node src/app.js". Diagnose the cause and fix it so the program runs successfully and prints its expected output. Do not rewrite the program from scratch — find the actual bug.`,
    setup: () => ({
      'src/app.js': `import { readFileSync } from 'node:fs';\n\nfunction loadConfig() {\n  return JSON.parse(readFileSync('config.json', 'utf8'));\n}\n\nfunction main() {\n  const config = loadConfig();\n  const greet = config.greeting ?? 'hello';\n  console.log(greet + ' ' + config.name);\n}\n\nmain();\n`,
      'config.json': `{\n  "name": "world"\n  "greeting": "hi"\n}\n`,
    }),
    verify: async (root, kernel) => {
      const run = await kernel.call('proc.spawn', { cmd: 'node', args: ['src/app.js'], cwd: '.', timeoutMs: 30_000 });
      const out = (run.result?.stdout || '').trim();
      // "hi world" is the expected output given the config (greeting "hi", name "world")
      const pass = run.ok && run.result.exitCode === 0 && out === 'hi world';
      return { pass, evidence: `exit=${run.result?.exitCode} stdout=${JSON.stringify(out)}` };
    },
  },
  {
    id: 'add-feature-with-test',
    category: 'edit',
    difficulty: 'medium',
    language: 'js',
    instruction: `src/stack.js implements a Stack class with push and pop. Add a peek() method that returns the top element WITHOUT removing it (returns undefined when empty), and an isEmpty() method returning a boolean. Then extend the existing test file src/stack.test.mjs with tests for both new methods and run the full test file to confirm everything passes.`,
    setup: () => ({
      'src/stack.js': `export class Stack {\n  #items = [];\n\n  push(item) {\n    this.#items.push(item);\n  }\n\n  pop() {\n    return this.#items.pop();\n  }\n\n  get size() {\n    return this.#items.length;\n  }\n}\n`,
      'src/stack.test.mjs': `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { Stack } from './stack.js';\n\ntest('push and pop', () => {\n  const s = new Stack();\n  s.push(1);\n  s.push(2);\n  assert.equal(s.pop(), 2);\n  assert.equal(s.pop(), 1);\n});\n\ntest('size', () => {\n  const s = new Stack();\n  assert.equal(s.size, 0);\n  s.push('x');\n  assert.equal(s.size, 1);\n});\n`,
    }),
    verify: async (root, kernel) => {
      const run = await kernel.call('proc.spawn', {
        cmd: 'node', args: ['--test', 'src/stack.test.mjs'], cwd: '.', timeoutMs: 60_000,
      });
      const src = await kernel.call('fs.read', { path: 'src/stack.js' });
      const hasPeek = /peek/.test(src.result?.content || '');
      const hasIsEmpty = /isEmpty/.test(src.result?.content || '');
      const pass = run.ok && run.result.exitCode === 0 && hasPeek && hasIsEmpty;
      return { pass, evidence: `tests: ${(run.result?.stdout || run.result?.stderr || '').slice(-300)}; peek=${hasPeek} isEmpty=${hasIsEmpty}` };
    },
  },
];

import { tier3 } from './tier3.mjs';

export const tasks = [...tier1, ...tier2, ...tier3,
  { // semantic-locate and web-server-control are inline in tier2's export; see below
    id: 'semantic-locate',
    category: 'investigate',
    difficulty: 'medium',
    language: 'js',
    instruction: `The app's business logic lives across many files in src/. Find the code that calculates a customer's ENTIRE ORDER TOTAL including tax and discounts — NOT the code that computes tax alone, and NOT the code that applies coupons to a single line item — and tell me the file path and the function name. Do this efficiently: try to find it WITHOUT reading every file one by one (a semantic search tool exists). You MUST use search.semantic at least once. Reply with the exact path like src/xxx/yyy.js and the function name.`,
    setup: () => ({
      'src/billing/tax.js': `export function computeTax(amount) {\n  return amount * 0.18;\n}\n`,
      'src/billing/lineitem.js': `export function applyCoupon(price, coupon) {\n  return coupon.valid ? price * (1 - coupon.percent / 100) : price;\n}\n`,
      'src/billing/shipping.js': `export function shippingCost(items) {\n  return items.length === 0 ? 0 : 5.5;\n}\n`,
      'src/billing/order-total.js': `import { computeTax } from './tax.js';\nimport { shippingCost } from './shipping.js';\n\n// The whole order: subtotal, coupons per line, tax, shipping\nexport function calculateOrderTotal(items, coupons) {\n  const subtotal = items.reduce((sum, it) => sum + it.price, 0);\n  const discounted = items.reduce((sum, it, i) => sum + applyCoupon(it.price, coupons[i] ?? {}), 0);\n  const tax = computeTax(discounted);\n  return discounted + tax + shippingCost(items);\n}\n`,
    }),
    verify: async (root, kernel, finalText = '', mode = 'kernel') => {
      // Ground truth for this task (deterministic — the task is fixed):
      // the ONLY module that computes the whole order total with tax+discounts.
      const GROUND_TRUTH = { path: 'src/billing/order-total.js', fn: 'calculateOrderTotal' };
      const pathMatch = finalText.match(/src\/[A-Za-z0-9_./-]+\.js/);
      // function name: prefer the backticked token that follows the word
      // "function"/"Function"; otherwise the first backticked identifier
      // that looks like a camelCase function and isn't a path fragment.
      const fnFollow = finalText.match(/`([A-Za-z_][A-Za-z0-9_]*)`\s*(?:that|computes|returns|is|which)?[^`\n]*/i);
      let answerFn = fnFollow ? fnFollow[1] : null;
      const backticked = finalText.match(/`([A-Za-z_][A-Za-z0-9_]*)`/g);
      const candidates = (backticked || []).map((t) => t.slice(1, -1)).filter((n) =>
        /^[A-Za-z_][A-Za-z0-9_]*$/.test(n) && !['name', 'path', 'file', 'function', 'js', 'coupons', 'items', 'tax'].includes(n.toLowerCase()));
      if (!answerFn || candidates.length === 0) {
        // fall back: the token right after "function"
        const m = finalText.match(/function\s+`?([A-Za-z_][A-Za-z0-9_]*)`?/i);
        answerFn = m && m[1] !== 'name' ? m[1] : (candidates.length ? candidates[candidates.length - 1] : null);
      }
      const answerPath = pathMatch ? pathMatch[0] : null;
      // correctness: answer must name the ground-truth module + function
      const correct = answerPath === GROUND_TRUTH.path && answerFn === GROUND_TRUTH.fn;
      // disk proof: the named file exists AND contains the named function
      let onDisk = false;
      if (answerPath && answerFn) {
        const st = await kernel.call('fs.stat', { path: answerPath });
        if (st.result?.exists) {
          const read = await kernel.call('fs.read', { path: answerPath });
          onDisk = new RegExp(`function\\s+${answerFn}|export\\s+function\\s+${answerFn}|const\\s+${answerFn}\\s*=`).test(read.result?.content || '');
        }
      }
      const journal = await kernel.journal.readAll();
      // usedSemantic is only a hard requirement when the agent actually has
      // the tool (kernel arm). The bash arm has no search.semantic tool — if
      // the journal shows one anyway, the model bypassed the harness by
      // importing the kernel module directly: report it, don't count it.
      const semanticViaAgent = journal.some((e) => e.kind === 'tool.call' && e.tool === 'search.semantic');
      const bypass = mode === 'bash' && semanticViaAgent;
      const pass = correct && onDisk && (mode === 'bash' ? true : semanticViaAgent);
      const reason = !correct ? 'wrong target (must be the order-total module)'
        : !onDisk ? 'answer not on disk'
        : mode === 'kernel' && !semanticViaAgent ? 'kernel arm never used search.semantic'
        : bypass ? 'bash arm bypassed the harness (kernel module import)'
        : 'verified';
      return { pass, evidence: `answer=${answerPath}/${answerFn}; correct=${correct}; onDisk=${onDisk}; semanticInJournal=${semanticViaAgent}${bypass ? '; HARNESS-BYPASS DETECTED' : ''} (${reason})` };
    },
  },
  // Wave-2 demo: the workflow that historically required terminal tabs and curl.
  {
    id: 'web-server-control',
    category: 'control',
    difficulty: 'hard',
    language: 'js',
    instruction: `The workspace contains src/server.js, an HTTP server. Your job — without ever using proc.spawn for the server itself:
1. Start the server as a background process using proc.start (it listens on port 4123).
2. Wait until it is actually serving: poll net.probePort until port 4123 is open.
3. Verify it works: use net.http to GET http://127.0.0.1:4123/ping and confirm the response body contains "pong".
4. Stop the server with proc.stop.
5. Confirm the port is closed afterwards (net.probePort must report open=false).
Report each step's result in your final answer.`,
    setup: () => ({
      'src/server.js': `const http = require('node:http');
const s = http.createServer((req, res) => {
  if (req.url === '/ping') { res.end('pong'); return; }
  res.end('hello');
});
s.listen(4123, () => console.log('listening on 4123'));
`,
    }),
    verify: async (root, kernel) => {
      const journal = await kernel.journal.readAll();
      const calls = journal.filter((e) => e.kind === 'tool.call');
      const results = journal.filter((e) => e.kind === 'tool.result');
      const resultFor = (seq) => results.find((r) => r.callSeq === seq);
      // 1. server was started with proc.start (NOT proc.spawn)
      const startCall = calls.find((e) => e.tool === 'proc.start' && e.args?.cmd === 'node' && (e.args?.args ?? []).includes('src/server.js'));
      // 2. port was probed
      const probeCall = calls.find((e) => e.tool === 'net.probePort' && e.args?.port === 4123);
      // 3. HTTP GET succeeded with pong body
      const httpCall = calls.find((e) => e.tool === 'net.http' && /4123\/ping/.test(e.args?.url ?? ''));
      const httpRes = httpCall ? resultFor(httpCall.seq) : null;
      const httpOk = httpRes?.ok && httpRes.result?.status === 200 && /pong/.test(httpRes.result?.body ?? '');
      // 4. server was stopped
      const stopCall = startCall ? calls.find((e) => e.tool === 'proc.stop' && resultFor(e.seq)?.ok) : null;
      // 5. port closed at the end — verifier checks live, right now
      const closedNow = await kernel.call('net.probePort', { port: 4123, timeoutMs: 800 });
      const noSpawnServer = !calls.some((e) => e.tool === 'proc.spawn' && (e.args?.args ?? []).some((a) => String(a).includes('server.js')));
      const pass = !!(startCall && probeCall && httpOk && stopCall && closedNow.result?.open === false && noSpawnServer);
      return {
        pass,
        evidence: `start=${!!startCall} probe=${!!probeCall} httpOk=${httpOk} stopped=${!!stopCall} portClosedNow=${closedNow.result?.open === false} noSpawnForServer=${noSpawnServer}`,
      };
    },
  },
];
