// Tier 3: diverse, laddered tasks (easy → expert) across languages and
// surfaces. Verifiers are all real: they execute code, check on-disk state,
// or walk the journal. No self-reporting.

import { writeFileSync, mkdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';

export const tier3 = [
  // ---------- EASY: single-file, single-step ----------
  {
    id: 'py-fix-slice',
    difficulty: 'easy',
    language: 'python',
    category: 'fix',
    instruction: `src/stats.py has a bug: first_two(items) should return the FIRST TWO elements of items, but it's wrong. Fix it so first_two([1,2,3]) returns [1,2]. Do not change the function name. The test file tests/test_stats.py must pass when run with: python -m pytest tests/ -q`,
    setup: () => ({
      'src/stats.py': `def first_two(items):\n    return items[-2:]\n`,
      'tests/test_stats.py': `def test_first_two():\n    from src.stats import first_two\n    assert first_two([1, 2, 3]) == [1, 2]\n    assert first_two([7]) == [7]\n    assert first_two([]) == []\n`,
    }),
    verify: async (root, kernel) => {
      const r = await kernel.call('test.run', { framework: 'pytest', path: 'tests/' });
      return { pass: r.ok && r.result.exitCode === 0 && r.result.failed === 0,
        evidence: (r.result?.stdout || r.result?.stderr || r.error?.message || '') };
    },
  },
  {
    id: 'rename-python',
    difficulty: 'easy',
    language: 'python',
    category: 'refactor',
    instruction: `src/util.py defines a function fetch_data that is used in src/client.py. Rename fetch_data to load_data everywhere (definition + call site). The test file must still pass.`,
    setup: () => ({
      'src/util.py': `def fetch_data(url):\n    return f"data-from-{url}"\n`,
      'src/client.py': `from src.util import fetch_data\n\n\ndef grab():\n    return fetch_data("api.example.com")\n`,
      'tests/test_util.py': `def test_grab():\n    from src.client import grab\n    assert grab() == "data-from-api.example.com"\n    from src.util import load_data\n    assert load_data("x") == "data-from-x"\n`,
    }),
    verify: async (root, kernel) => {
      const grep = await kernel.call('search.grep', { pattern: 'fetch_data', path: 'src/' });
      const r = await kernel.call('test.run', { framework: 'pytest', path: 'tests/' });
      const noLeftover = grep.result.matches.length === 0;
      return { pass: noLeftover && r.ok && r.result?.failed === 0,
        evidence: JSON.stringify({ leftover: grep.result.matches.length, tests: r.result }) };
    },
  },
  // ---------- MEDIUM: 2-3 steps, some exploration ----------
  {
    id: 'py-csv-summary',
    difficulty: 'medium',
    language: 'python',
    category: 'create',
    instruction: `Create a Python script src/csv_summary.py that reads data/sales.csv (columns: region,amount) and prints ONE line per region with the total sales for that region, highest region first. Run it and verify the output matches: west 250, east 120. Keep CSV parsing in pure Python (no pandas).`,
    setup: () => ({
      'data/sales.csv': `region,amount\neast,70\nwest,100\nwest,150\neast,50\n`,
    }),
    verify: async (root, kernel) => {
      const r = await kernel.call('proc.spawn', { cmd: 'python', args: ['src/csv_summary.py'], timeoutMs: 30_000 });
      const lines = (r.result?.stdout || '').trim().split('\n').map((l) => l.trim());
      const west = lines.filter((l) => l.startsWith('west')).map((l) => Number(l.split(/\s+/).pop())).reduce((a, b) => Math.max(a, b), 0);
      const east = Number(lines.find((l) => l.startsWith('east'))?.split(/\s+/).pop() ?? 0);
      const westTotal = lines.filter((l) => l.startsWith('west')).length;
      const pass = west >= 250 && east === 120 && lines.findIndex((l) => l.startsWith('west')) < lines.findIndex((l) => l.startsWith('east'));
      return { pass, evidence: JSON.stringify({ exit: r.result?.exitCode, lines }) };
    },
  },
  {
    id: 'fix-js-import-cycle',
    difficulty: 'medium',
    language: 'js',
    category: 'fix',
    instruction: `The app crashes on startup with an import error even though every file exists. Diagnose the REAL cause (read the imports carefully) and fix it so "node src/app.js" prints "started".`,
    setup: () => ({
      'src/app.js': `import { start } from './logger.js';\nstart();\nconsole.log('started');\n`,
      // circular-ish import mistake: logger imports from a file that imports back
      'src/logger.js': `import { config } from './config.j';\nexport function start() {\n  console.log('logger:', config.name);\n}\n`,
      'src/config.js': `export const config = { name: 'ok' };\n`,
    }),
    verify: async (root, kernel) => {
      const r = await kernel.call('proc.spawn', { cmd: 'node', args: ['src/app.js'], timeoutMs: 30_000 });
      return { pass: r.ok && r.result.exitCode === 0 && (r.result.stdout || '').includes('started'),
        evidence: JSON.stringify({ exit: r.result?.exitCode, out: (r.result?.stdout || '').slice(0, 120), err: (r.result?.stderr || '').slice(0, 120) }) };
    },
  },
  // ---------- HARD: multi-step, cross-cutting ----------
  {
    id: 'git-multi-commit',
    difficulty: 'hard',
    language: 'js',
    category: 'investigate',
    instruction: `The repo has multiple commits in its history. A regression was introduced in an EARLIER commit and now src/app.js prints the wrong value. Investigate with git.log and git.diff, find which commit changed the LIMIT constant to the wrong value, then fix the CURRENT code so "node src/app.js" prints "ok:42". You may use git.status, git.diff, git.log, patch.apply.`,
    setup: (root) => {
      const write = (content) => {
        mkdirSync(`${root}/src`, { recursive: true });
        writeFileSync(`${root}/src/app.js`, content, 'utf8');
      };
      const git = (...args) => {
        const r = spawnSync('git', args, { cwd: root, encoding: 'utf8' });
        if (r.status !== 0) throw new Error(`git ${args[0]}: ${r.stderr}`);
        return r.stdout;
      };
      write('const LIMIT = 7;\nconsole.log(\'ok:\' + LIMIT * 6);\n');
      git('add', 'src/app.js'); git('commit', '-m', 'good state');
      write('const LIMIT = 2;\nconsole.log(\'ok:\' + LIMIT * 6);\n');
      git('add', 'src/app.js'); git('commit', '-m', 'change perf (bug source)');
      write('const LIMIT = 3;\nconsole.log(\'ok:\' + LIMIT * 6);\n');
      git('add', 'src/app.js'); git('commit', '-m', 'add feature');
      return {};
    },
    verify: async (root, kernel) => {
      const r = await kernel.call('proc.spawn', { cmd: 'node', args: ['src/app.js'], timeoutMs: 30_000 });
      const pass = r.ok && r.result.exitCode === 0 && (r.result.stdout || '').trim() === 'ok:42';
      return { pass, evidence: JSON.stringify({ exit: r.result?.exitCode, out: (r.result?.stdout || '').trim() }) };
    },
  },
  {
    id: 'env-config-app',
    difficulty: 'hard',
    language: 'js',
    category: 'control',
    instruction: `src/server.js reads its port and response text from environment variables (PORT and SAY). Start it as a background process (proc.start) with env.set so it listens on port 4235, then use net.http to GET http://127.0.0.1:4235/ and confirm the body equals the value you set for SAY, then stop it. All env setup must happen via env.set before the process starts.`,
    setup: () => ({
      'src/server.js': `const http = require('node:http');\nconst port = Number(process.env.PORT || 3000);\nconst say = process.env.SAY || 'default';\nhttp.createServer((req, res) => res.end(say)).listen(port, () => console.log('up on ' + port));\n`,
    }),
    verify: async (root, kernel) => {
      const journal = kernel.journal.readAll();
      const calls = journal.filter((e) => e.kind === 'tool.call');
      const results = journal.filter((e) => e.kind === 'tool.result');
      const resultFor = (s) => results.find((r) => r.callSeq === s);
      const envSet = calls.find((e) => e.tool === 'env.set' && e.args?.name === 'PORT' && e.args?.value === '4235');
      const envSay = calls.find((e) => e.tool === 'env.set' && e.args?.name === 'SAY');
      const startCall = calls.find((e) => e.tool === 'proc.start');
      const stopCall = startCall ? calls.find((e) => e.tool === 'proc.stop' && resultFor(e.seq)?.ok) : null;
      const httpCall = calls.find((e) => e.tool === 'net.http' && /4235/.test(e.args?.url ?? ''));
      const httpRes = httpCall ? resultFor(httpCall.seq) : null;
      const bodyOk = httpRes?.ok && httpRes.result?.status === 200 && envSay && httpRes.result?.body === envSay.args.value;
      const closedNow = await kernel.call('net.probePort', { port: 4235, timeoutMs: 800 });
      const pass = !!(envSet && startCall && stopCall && bodyOk && closedNow.result?.open === false);
      return { pass, evidence: JSON.stringify({ envSet: !!envSet, start: !!startCall, stop: !!stopCall, bodyOk, portClosed: closedNow.result?.open === false }) };
    },
  },
  {
    id: 'multi-step-tdd',
    difficulty: 'hard',
    language: 'js',
    category: 'create',
    instruction: `Step 1: write a failing test first (tests/fizz.test.mjs) for fizzBuzz(n) with the classic rules (divisible by 3 → "Fizz", by 5 → "Buzz", by both → "FizzBuzz", else the number as a string), for n = 3, 5, 15, 7. Step 2: run it and confirm it fails (see the failure identities with test.run). Step 3: implement src/fizz.js so the tests pass. Step 4: run the full test again to confirm green. This is a test-driven task — the test file must exist before the implementation. Report the failing-then-passing sequence.`,
    setup: () => ({ 'src/.keep': '' }),
    verify: async (root, kernel) => {
      const r = await kernel.call('test.run', { framework: 'node', path: 'tests/fizz.test.mjs' });
      const impl = await kernel.call('fs.stat', { path: 'src/fizz.js' });
      const testFile = await kernel.call('fs.stat', { path: 'tests/fizz.test.mjs' });
      // TDD order proof, arm-neutral: the test file must exist and have been
      // written BEFORE the implementation (mtime test <= impl, +1s tolerance
      // for same-millisecond writes). Journal-based ordering would bias
      // against the bash arm, which writes files via proc.spawn, not fs.write.
      let tddOrder = false;
      if (testFile.result?.exists && impl.result?.exists) {
        tddOrder = testFile.result.mtimeMs <= impl.result.mtimeMs + 1000;
      }
      const pass = r.ok && r.result?.failed === 0 && r.result?.passed >= 1 && impl.result?.exists && tddOrder;
      return { pass, evidence: JSON.stringify({ tests: r.result?.passed, failed: r.result?.failed, impl: !!impl.result?.exists, tddOrder }) };
    },
  },
  // ---------- EXPERT: long-horizon, stateful ----------
  {
    id: 'expert-refactor-lib',
    difficulty: 'expert',
    language: 'js',
    category: 'refactor',
    instruction: `src/ has four modules each defining its own clamp (buggy or duplicated). Consolidate into src/mathlib.js exporting clamp(value, min, max): returns max if value > max, min if value < min, else value; when min > max or value is NaN, return NaN. Update a.js b.js c.js d.js so each imports clamp from './mathlib.js' (correct relative import) and stops defining it, while still exporting their other functions (pad, id, double, names). Do not edit tests/. Run: node --test tests/math.test.mjs and make it green. Report changed files.`,
    setup: () => ({
      'src/a.js': `export function clamp(v, min, max) {\n  return v > max ? max : v < min ? max : v;\n}\nexport function pad(n) { return String(n).padStart(2, '0'); }\n`,
      'src/b.js': `export function clamp(v, min, max) {\n  return Math.max(min, Math.min(max, v - 1));\n}\nexport function id(x) { return x; }\n`,
      'src/c.js': `export const clamp = (v, min, max) => Math.min(max, Math.max(min, v));\nexport function double(x) { return x * 2; }\n`,
      'src/d.js': `export function clamp(v, min, max) {\n  return v;\n}\nexport function names() { return ['a', 'b']; }\n`,
      'tests/math.test.mjs': `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { clamp } from '../src/mathlib.js';\nimport { pad } from '../src/a.js';\nimport { id } from '../src/b.js';\nimport { double } from '../src/c.js';\nimport { names } from '../src/d.js';\n\ntest('clamp basic', () => {\n  assert.equal(clamp(2, 1, 3), 2);\n  assert.equal(clamp(0, 1, 3), 1);\n  assert.equal(clamp(5, 1, 3), 3);\n  assert.equal(Number.isNaN(clamp(2, 3, 1)), true);\n});\n\ntest('siblings keep working', () => {\n  assert.equal(pad(7), '07');\n  assert.equal(id(9), 9);\n  assert.equal(double(4), 8);\n  assert.equal(names().length, 2);\n});\n`,
    }),
    verify: async (root, kernel) => {
      const r = await kernel.call('proc.spawn', { cmd: 'node', args: ['--test', 'tests/math.test.mjs'], timeoutMs: 60_000 });
      const grep = await kernel.call('search.grep', { pattern: 'function clamp|const clamp|clamp =', path: 'src/' });
      const leftovers = grep.result.matches.filter((m) => !m.file.includes('mathlib.js') && !m.file.includes('test'));
      const mathlib = await kernel.call('fs.stat', { path: 'src/mathlib.js' });
      const pass = r.ok && r.result.exitCode === 0 && leftovers.length === 0 && mathlib.result?.exists;
      return { pass, evidence: JSON.stringify({ test: (r.result?.stdout || '').slice(-90), leftovers: leftovers.map((m) => m.file), mathlib: !!mathlib.result?.exists }) };
    },
  },
];

export const allTasks = [...tier3];
