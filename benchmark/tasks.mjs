// Benchmark tasks. Each: id, category, instruction, setup (files written to a
// fresh workspace), verify (async (root, kernel) => {pass, evidence}).
// Verifiers use the real filesystem — no self-reporting.

export const tasks = [
  {
    id: 'fix-off-by-one',
    category: 'fix',
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
