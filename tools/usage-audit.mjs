// Usage-gap analyzer: aggregates every nc-tools journal on disk (benchmark
// transcripts + dogfooding) into tool-frequency, error, and coverage tables.
// Answers: which of the 48 tools do agents actually call, which do they
// attempt-but-fail, which do they try to use and don't have?
//
// Usage: node tools/usage-audit.mjs [dir...]
//        dirs are searched recursively for *.journal.jsonl / journal.jsonl
import { readdirSync, readFileSync, statSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { dirname } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const defaultDirs = [join(root, 'benchmark', 'results'), join(root, '.nc-tools')];
const dirs = process.argv.slice(2).length ? process.argv.slice(2).map((d) => resolve(d)) : defaultDirs;

const files = [];
function walk(d) {
  if (!existsSync(d)) return;
  for (const e of readdirSync(d, { withFileTypes: true })) {
    const p = join(d, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.isFile() && e.name.endsWith('.jsonl') && e.name.includes('journal')) files.push(p);
  }
}
for (const d of dirs) walk(d);

// golden tool list = ground truth for coverage
const golden = JSON.parse(readFileSync(join(root, 'conformance', 'golden', 'tools.json'), 'utf8'));
const allTools = golden.tools.map((t) => t.name);

const calls = new Map();      // tool -> count of tool_result events
const errors = new Map();     // error code -> count
const errorByTool = new Map();// tool -> Map(code -> n)
const unknownAttempts = [];   // attempts to call tools that don't exist
const callArgSamples = new Map(); // tool -> few sample first-args (keys used)
let totalEvents = 0;

for (const f of files) {
  const lines = readFileSync(f, 'utf8').split('\n').filter(Boolean);
  for (const line of lines) {
    let e; try { e = JSON.parse(line); } catch { continue; }
    totalEvents++;
    const tool = e.tool ?? e.name ?? e.call?.tool;
    if (tool) {
      calls.set(tool, (calls.get(tool) ?? 0) + 1);
      if (e.error?.code) {
        errors.set(e.error.code, (errors.get(e.error.code) ?? 0) + 1);
        if (!errorByTool.has(tool)) errorByTool.set(tool, new Map());
        const m = errorByTool.get(tool);
        m.set(e.error.code, (m.get(e.error.code) ?? 0) + 1);
      }
      if (!callArgSamples.has(tool) && e.args && typeof e.args === 'object') {
        callArgSamples.set(tool, Object.keys(e.args).slice(0, 5).join(','));
      }
    }
    // unknown-tool attempts appear as tool_result with ERR_UNKNOWN_TOOL
    if (e.error?.code === 'ERR_UNKNOWN_TOOL' && e.tool) unknownAttempts.push(e.tool);
  }
}

const totalCalls = [...calls.values()].reduce((a, b) => a + b, 0);
console.log(`journals: ${files.length} files, ${totalEvents} events, ${totalCalls} tool calls\n`);

console.log('=== TOOL FREQUENCY (desc) ===');
const sorted = [...calls.entries()].sort((a, b) => b[1] - a[1]);
for (const [t, n] of sorted) {
  const errN = [...(errorByTool.get(t) ?? new Map()).values()].reduce((a, b) => a + b, 0);
  console.log(`${t.padEnd(24)} ${String(n).padStart(5)}  (${((n / totalCalls) * 100).toFixed(1)}%)${errN ? `  errors: ${errN}` : ''}`);
}

const never = allTools.filter((t) => !calls.has(t));
console.log(`\n=== NEVER CALLED (${never.length}/${allTools.length}) ===`);
console.log(never.join(', ') || '(none)');

console.log('\n=== ERROR CODES (desc) ===');
for (const [c, n] of [...errors.entries()].sort((a, b) => b[1] - a[1])) console.log(`${c.padEnd(24)} ${n}`);
if (!errors.size) console.log('(none)');

if (unknownAttempts.length) {
  console.log('\n=== TOOLS AGENTS TRIED THAT DO NOT EXIST ===');
  const uniq = {};
  for (const t of unknownAttempts) uniq[t] = (uniq[t] ?? 0) + 1;
  for (const [t, n] of Object.entries(uniq).sort((a, b) => b[1] - a[1])) console.log(`${t.padEnd(24)} ${n} attempts`);
}