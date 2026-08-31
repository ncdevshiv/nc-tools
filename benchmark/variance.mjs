// Variance analyzer: for cells that have been run multiple times (repeat flag),
// report solve-rate stability and spread in tokens/time.
// Usage: node benchmark/variance.mjs <resultsDir...>
import { readFileSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

const dirs = process.argv.slice(2).map((d) => resolve(d));
const records = [];
for (const d of dirs) {
  const f = join(d, 'summary.json');
  if (existsSync(f)) records.push(...JSON.parse(readFileSync(f, 'utf8')));
}

const cells = new Map();
for (const r of records) {
  const k = `${r.task}|${r.model}|${r.mode}`;
  cells.set(k, (cells.get(k) ?? []).concat(r));
}

console.log('=== VARIANCE (cells with repeats) ===');
let anyRepeated = false;
for (const [k, runs] of cells) {
  if (runs.length < 2) continue;
  anyRepeated = true;
  const solved = runs.filter((r) => r.solved).length;
  const toks = runs.map((r) => r.totalTokens ?? 0).sort((a, b) => a - b);
  const ms = runs.map((r) => r.wallMs).sort((a, b) => a - b);
  const spread = (arr) => arr.length > 1 ? Math.round(((arr[arr.length - 1] - arr[0]) / (arr[0] || 1)) * 100) : 0;
  const outcomes = runs.map((r) => (r.solved ? 'PASS' : 'FAIL')).join(',');
  console.log(`${k.padEnd(40)} n=${runs.length} ${solved}/${runs.length} (${outcomes}) | tok ${toks[0]}-${toks[toks.length - 1]} (${spread(toks)}% spread) | ${(ms[0] / 1000).toFixed(0)}-${(ms[ms.length - 1] / 1000).toFixed(0)}s`);
}
if (!anyRepeated) console.log('no repeated cells found');
