// Canonical solutions for every benchmark task, used by validate-verifiers.mjs
// to prove each verifier passes the CORRECT solution and rejects WRONG ones.
// Each solution is a function (root, kernel) that performs edits with kernel
// tools — the same way an agent would.
import { writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const write = (root, p, content) => {
  mkdirSync(join(root, p, '..'), { recursive: true });
  writeFileSync(join(root, p), content, 'utf8');
};

export const solutions = {
  'fix-off-by-one': async (root, k) => {
    await k.call('patch.apply', { path: 'src/range.js', edits: [{ oldText: 'return arr.slice(0, n);', newText: 'return arr.slice(-n > -arr.length ? arr.length - n : 0);' }] });
  },
  'rename-function': async (root, k) => {
    await k.call('patch.applyMany', { edits: [
      { path: 'src/calc.js', edits: [{ oldText: 'computeTotal', newText: 'calculateTotal', expectedCount: 2 }] },
      { path: 'src/report.js', edits: [{ oldText: 'computeTotal', newText: 'calculateTotal', expectedCount: 2 }] },
    ] });
  },
  'find-and-fix-bug': async (root, k) => {
    await k.call('patch.apply', { path: 'config.json', edits: [{ oldText: '"name": "world"\n  "greeting": "hi"', newText: '"name": "world",\n  "greeting": "hi"' }] });
  },
  'add-feature-with-test': async (root, k) => {
    await k.call('patch.apply', {
      path: 'src/stack.js',
      edits: [
        { oldText: '  pop() {\n    return this.#items.pop();\n  }', newText: '  pop() {\n    return this.#items.pop();\n  }\n\n  peek() {\n    return this.#items.length ? this.#items[this.#items.length - 1] : undefined;\n  }\n\n  isEmpty() {\n    return this.#items.length === 0;\n  }' },
      ],
    });
    await k.call('patch.apply', {
      path: 'src/stack.test.mjs',
      edits: [
        { oldText: "test('size', () => {", newText: "test('peek and isEmpty', () => {\n  const s = new Stack();\n  assert.equal(s.isEmpty(), true);\n  assert.equal(s.peek(), undefined);\n  s.push('top');\n  assert.equal(s.peek(), 'top');\n  assert.equal(s.isEmpty(), false);\n  assert.equal(s.size, 1);\n});\n\ntest('size', () => {" },
      ],
    });
  },
  'implement-fn-from-spec': async (root, k) => {
    await k.call('fs.write', {
      path: 'src/utils/debounce.js',
      content: `export default function debounce(fn, waitMs) {\n  let timer = null;\n  let lastArgs = null;\n  let lastThis = null;\n  return function (...args) {\n    lastArgs = args;\n    lastThis = this;\n    clearTimeout(timer);\n    timer = setTimeout(() => {\n      fn.apply(lastThis, lastArgs);\n      timer = null;\n    }, waitMs);\n  };\n}\n`,
    });
    await k.call('fs.write', {
      path: 'src/utils/debounce.test.mjs',
      content: `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport debounce from './debounce.js';\n\ntest('collapses rapid calls into one', async () => {\n  const start = Date.now();\n  let calls = 0;\n  const fn = debounce(() => { calls += 1; }, 30);\n  fn(); fn(); fn();\n  await new Promise((r) => setTimeout(r, 60));\n  assert.equal(calls, 1);\n  assert.ok(Date.now() - start >= 30);\n});\n\ntest('last call wins', async () => {\n  let received = null;\n  const fn = debounce((x) => { received = x; }, 20);\n  fn('a'); fn('b');\n  await new Promise((r) => setTimeout(r, 50));\n  assert.equal(received, 'b');\n});\n`,
    });
  },
  'multi-file-refactor': async (root, k) => {
    await k.call('fs.write', {
      path: 'src/duration.js',
      content: `export function parseDuration(text) {\n  const m = text.match(/^(\\d+)([smh]?)$/);\n  if (!m) return Number.isFinite(Number(text)) ? Number(text) : NaN;\n  const n = Number(m[1]);\n  return m[2] === 'm' ? n * 60 : m[2] === 'h' ? n * 3600 : n;\n}\n`,
    });
    await k.call('fs.write', { path: 'src/audio.js', content: `import { parseDuration } from './duration.js';\n\nexport { parseDuration };\n\nexport function clipLength(t) {\n  return parseDuration(t) * 1000;\n}\n` });
    await k.call('fs.write', { path: 'src/scheduler.js', content: `import { parseDuration } from './duration.js';\n\nexport function scheduleAt(text, start) {\n  return start + parseDuration(text) * 1000;\n}\n` });
  },
  'fix-trap': async (root, k) => {
    await k.call('patch.apply', { path: 'src/server.js', edits: [{ oldText: "  const warmup = createListener(3000);\n  if (!warmup) throw new Error('warmup failed');", newText: "  const warmup = createListener(3000);\n  warmup.close();" }] });
  },
  'tdd-implement': async (root, k) => {
    await k.call('fs.write', {
      path: 'src/roman.js',
      content: `const ROMAN = [[1000, 'M'], [900, 'CM'], [500, 'D'], [400, 'CD'], [100, 'C'], [90, 'XC'], [50, 'L'], [40, 'XL'], [10, 'X'], [9, 'IX'], [5, 'V'], [4, 'IV'], [1, 'I']];\n\nexport function toRoman(n) {\n  if (!Number.isInteger(n) || n < 0 || n > 3999) return '';\n  let out = '';\n  let v = n;\n  for (const [val, sym] of ROMAN) {\n    while (v >= val) { out += sym; v -= val; }\n  }\n  return out;\n}\n\nexport function fromRoman(s) {\n  const map = { I: 1, V: 5, X: 10, L: 50, C: 100, D: 500, M: 1000 };\n  let total = 0;\n  for (let i = 0; i < s.length; i++) {\n    const cur = map[s[i]] ?? 0;\n    const next = map[s[i + 1]] ?? 0;\n    total += cur < next ? -cur : cur;\n  }\n  return total;\n}\n`,
    });
    await k.call('fs.write', {
      path: 'test/roman.test.mjs',
      content: `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { toRoman, fromRoman } from '../oracle/roman.js';\n\ntest('basic conversions', () => {\n  assert.equal(toRoman(9), 'IX');\n  assert.equal(toRoman(2024), 'MMXXIV');\n  assert.equal(toRoman(0), '');\n  assert.equal(fromRoman('XIV'), 14);\n  assert.equal(fromRoman('MMXXIV'), 2024);\n});\n\ntest('round-trip for several values', () => {\n  for (const n of [0, 1, 42, 999, 3000, 1987]) {\n    assert.equal(fromRoman(toRoman(n)), n);\n  }\n});\n`,
    });
  },
  'web-server-control': async (root, k) => {
    const started = await k.call('proc.start', { cmd: 'node', args: ['src/server.js'] });
    for (let i = 0; i < 25; i++) {
      await sleep(400);
      const p = await k.call('net.probePort', { port: 4123 });
      if (!p.result.open) continue;
      const h = await k.call('net.http', { url: 'http://127.0.0.1:4123/ping' });
      if (h.ok) break;
    }
    await k.call('proc.stop', { handleId: started.result.handleId });
  },
  'semantic-locate': async (root, k) => {
    await k.call('search.semantic', { query: 'order total', cacheDir: process.env.NCTOOLS_MODEL_CACHE });
  },
  'py-fix-slice': async (root, k) => {
    await k.call('patch.apply', { path: 'src/stats.py', edits: [{ oldText: 'return items[-2:]', newText: 'return items[:2]' }] });
  },
  'rename-python': async (root, k) => {
    await k.call('patch.applyMany', { edits: [
      { path: 'src/util.py', edits: [{ oldText: 'fetch_data', newText: 'load_data', expectedCount: 1 }] },
      { path: 'src/client.py', edits: [{ oldText: 'fetch_data', newText: 'load_data', expectedCount: 2 }] },
    ] });
  },
  'py-csv-summary': async (root, k) => {
    await k.call('fs.write', {
      path: 'src/csv_summary.py',
      content: `import csv\nfrom collections import defaultdict\n\nwith open('data/sales.csv') as f:\n    reader = csv.DictReader(f)\n    totals = defaultdict(int)\n    for row in reader:\n        totals[row['region']] += int(row['amount'])\n\nfor region in sorted(totals, key=lambda r: -totals[r]):\n    print(region, totals[region])\n`,
    });
  },
  'fix-js-import-cycle': async (root, k) => {
    await k.call('patch.apply', { path: 'src/logger.js', edits: [{ oldText: "from './config.j'", newText: "from './config.js'" }] });
  },
  'git-multi-commit': async (root, k) => {
    await k.call('patch.apply', { path: 'src/app.js', edits: [{ oldText: 'const LIMIT = 3;', newText: 'const LIMIT = 7;' }] });
  },
  'env-config-app': async (root, k) => {
    await k.call('env.set', { name: 'PORT', value: '4235' });
    await k.call('env.set', { name: 'SAY', value: 'hello-config' });
    const started = await k.call('proc.start', { cmd: 'node', args: ['src/server.js'] });
    for (let i = 0; i < 25; i++) {
      await sleep(400);
      const p = await k.call('net.probePort', { port: 4235 });
      if (!p.result.open) continue;
      const h = await k.call('net.http', { url: 'http://127.0.0.1:4235/' });
      if (h.ok && h.result.body === 'hello-config') break;
    }
    await k.call('proc.stop', { handleId: started.result.handleId });
  },
  'multi-step-tdd': async (root, k) => {
    await k.call('fs.write', {
      path: 'tests/fizz.test.mjs',
      content: `import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fizzBuzz } from '../oracle/fizz.js';

test('classic fizzbuzz rules', () => {
  assert.equal(fizzBuzz(3), 'Fizz');
  assert.equal(fizzBuzz(5), 'Buzz');
  assert.equal(fizzBuzz(15), 'FizzBuzz');
  assert.equal(fizzBuzz(7), '7');
});
`,
    });
    await k.call('fs.write', {
      path: 'src/fizz.js',
      content: `export function fizzBuzz(n) {
  if (n % 15 === 0) return 'FizzBuzz';
  if (n % 3 === 0) return 'Fizz';
  if (n % 5 === 0) return 'Buzz';
  return String(n);
}
`,
    });
  },
  'expert-refactor-lib': async (root, k) => {
    await k.call('fs.write', {
      path: 'src/mathlib.js',
      content: `export function clamp(value, min, max) {\n  if (min > max || Number.isNaN(value)) return NaN;\n  if (value > max) return max;\n  if (value < min) return min;\n  return value;\n}\n`,
    });
    await k.call('fs.write', { path: 'src/a.js', content: `import { clamp } from './mathlib.js';\nexport { clamp };\nexport function pad(n) { return String(n).padStart(2, '0'); }\n` });
    await k.call('fs.write', { path: 'src/b.js', content: `import { clamp } from './mathlib.js';\nexport { clamp };\nexport function id(x) { return x; }\n` });
    await k.call('fs.write', { path: 'src/c.js', content: `import { clamp } from './mathlib.js';\nexport { clamp };\nexport function double(x) { return x * 2; }\n` });
    await k.call('fs.write', { path: 'src/d.js', content: `import { clamp } from './mathlib.js';\nexport { clamp };\nexport function names() { return ['a', 'b']; }\n` });
  },
};

// Wrong solutions: a plausible-but-incorrect edit the verifier must REJECT.
// finalText to use when validating tasks whose verifier parses the agent's answer
export const canonicalAnswers = {
  'semantic-locate': 'Found it. **File:** `src/billing/order-total.js` | **Function:** `calculateOrderTotal`',
};

export const wrongSolutions = {
  'fix-off-by-one': async (root, k) => {
    await k.call('patch.apply', { path: 'src/range.js', edits: [{ oldText: 'return arr.slice(0, n);', newText: 'return arr.slice(n);' }] });
  },
  'py-fix-slice': async (root, k) => {
    await k.call('patch.apply', { path: 'src/stats.py', edits: [{ oldText: 'return items[-2:]', newText: 'return items[2:]' }] });
  },
  'find-and-fix-bug': async (root, k) => {
    await k.call('patch.apply', { path: 'config.json', edits: [{ oldText: '"name": "world"', newText: '"name": "world",\n  "greeting": "hi",\n  "broken": true' }] });
  },
  'git-multi-commit': async (root, k) => {
    await k.call('patch.apply', { path: 'src/app.js', edits: [{ oldText: 'const LIMIT = 3;', newText: 'const LIMIT = 8;' }] });
  },
};
