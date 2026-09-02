// Benchmark analyzer: turns result summaries into a scoreboard by task,
// difficulty tier, and language. Usage: node bench/analyze.mjs <resultsDir...>
import { readFileSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';

const dirs = process.argv.slice(2).map((d) => resolve(d));
if (!dirs.length) { console.error('usage: node bench/analyze.mjs <resultsDir...>'); process.exit(2); }

import { tasks } from './tasks.mjs';
const TASK_META = new Map(tasks.map((t) => [t.id, t]));

const records = [];
for (const d of dirs) {
  const f = join(d, 'summary.json');
  if (!existsSync(f)) { console.error(`no summary.json in ${d}`); continue; }
  records.push(...JSON.parse(readFileSync(f, 'utf8')));
}

if (!records.length) { console.error('no records found'); process.exit(1); }
for (const r of records) {
  const meta = TASK_META.get(r.task);
  if (meta) { r.difficulty = meta.difficulty ?? '?'; r.language = meta.language ?? '?'; }
}

// order tasks by difficulty ladder
const ORDER = { easy: 0, medium: 1, hard: 2, expert: 3 };
const byId = new Map();
for (const r of records) {
  const k = `${r.task}|${r.model.split('/')[0]}|${r.mode ?? 'kernel'}`;
  byId.set(k, r);
}

console.log('=== SCOREBOARD (per run) ===');
console.log(`${'task'.padEnd(24)} ${'diff'.padEnd(7)} ${'lang'.padEnd(6)} ${'model'.padEnd(14)} ${'arm'.padEnd(7)} ${'solved'.padEnd(6)} ${'calls'.padEnd(6)} ${'tok'.padEnd(9)} ${'wall_s'.padEnd(8)}`);
const seen = new Set();
for (const r of records) {
  const key = `${r.task}|${r.model}|${r.mode ?? 'kernel'}`;
  if (seen.has(key)) { continue; } // first occurrence only (per model/arm)
  seen.add(key);
  console.log(`${r.task.padEnd(24)} ${String(r.difficulty ?? '?').padEnd(7)} ${String(r.language ?? '?').padEnd(6)} ${r.model.includes('deepseek') ? 'deepseek'.padEnd(14) : r.model.split('/')[0].slice(0, 14).padEnd(14)} ${(r.mode ?? 'kernel').padEnd(7)} ${String(r.solved).padEnd(6)} ${String(r.toolCalls ?? '?').padEnd(6)} ${String(r.totalTokens ?? '?').padEnd(9)} ${((r.wallMs ?? 0) / 1000).toFixed(0).padEnd(8)}`);
}

console.log('\n=== BY DIFFICULTY TIER ===');
const tier = {};
for (const r of records) {
  if (seen.has(`${r.task}|${r.model}|${r.mode ?? 'kernel'}`) === false) {} // per-model aggregation below
}
for (const r of records) {
  const d = r.difficulty ?? '?';
  const k = `${d}|${r.model.includes('deepseek') ? 'deepseek' : 'glm'}|${r.mode ?? 'kernel'}`;
  tier[k] ??= { solved: 0, n: 0, calls: 0, errors: 0, tokens: 0, ms: 0 };
  const t = tier[k];
  t.n++; t.solved += r.solved ? 1 : 0; t.calls += r.toolCalls ?? 0; t.errors += r.toolErrors ?? 0;
  t.tokens += r.totalTokens ?? 0; t.ms += r.wallMs ?? 0;
}
for (const [k, t] of Object.entries(tier).sort((a, b) => a[0].localeCompare(b[0]))) {
  const [d, m, arm] = k.split('|');
  console.log(`${d.padEnd(7)} ${m.padEnd(9)} ${arm.padEnd(7)} ${t.solved}/${t.n} solved | ${t.calls} calls, ${t.errors} errored | ~${t.tokens} tok | ${(t.ms / 1000).toFixed(0)}s`);
}

console.log('\n=== BY LANGUAGE ===');
const lang = {};
for (const r of records) {
  const l = r.language ?? '?';
  const k = `${l}|${r.model.includes('deepseek') ? 'deepseek' : 'glm'}|${r.mode ?? 'kernel'}`;
  lang[k] ??= { solved: 0, n: 0 };
  lang[k].n++; lang[k].solved += r.solved ? 1 : 0;
}
for (const [k, v] of Object.entries(lang).sort()) console.log(`${k.padEnd(22)} ${v.solved}/${v.n} solved`);

console.log('\n=== GRAND TOTAL ===');
const total = records.length;
const solved = records.filter((r) => r.solved).length;
console.log(`${solved}/${total} runs solved (${Math.round((solved / total) * 100)}%)`);
const by = {};
for (const r of records) {
  const k = `${r.model.includes('deepseek') ? 'deepseek' : 'glm'}|${r.mode ?? 'kernel'}`;
  by[k] ??= { solved: 0, n: 0 };
  by[k].n++; by[k].solved += r.solved ? 1 : 0;
}
for (const [k, v] of Object.entries(by)) console.log(`  ${k}: ${v.solved}/${v.n}`);
