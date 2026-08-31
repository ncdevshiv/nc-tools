// Head-to-head comparison: identical tasks, identical models, identical
// verifier — one arm gets the typed kernel tools, the other gets a single
// `bash` tool. This is the "typed tools vs terminal" experiment.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../oracle/kernel/kernel.mjs';
import { runAgent } from '../oracle/agent/agent.mjs';
import { makeChat } from '../oracle/agent/llm.mjs';
import { tasks } from './tasks.mjs';
import { behaviorMetrics } from './metrics.mjs';

const baseURL = process.env.NCTOOLS_LLM_BASEURL;
const apiKey = process.env.NCTOOLS_LLM_APIKEY || '';
const models = (process.env.NCTOOLS_LLM_MODELS || '').split(',').map((s) => s.trim()).filter(Boolean);
const outDir = resolve(process.argv[2] || 'benchmark/results/compare');
const onlyTask = process.env.NCTOOLS_TASK || (process.env.NCTOOLS_TASKS ? null : null);
const onlyTasks = (process.env.NCTOOLS_TASKS || '').split(',').map((s) => s.trim()).filter(Boolean);
const repeatRuns = Math.max(1, Number(process.env.NCTOOLS_REPEATS || 1));

if (!baseURL || models.length === 0) {
  console.error('Usage: NCTOOLS_LLM_BASEURL=... NCTOOLS_LLM_APIKEY=... NCTOOLS_LLM_MODELS=a,b node benchmark/compare.mjs [outDir]');
  process.exit(2);
}

function setupWorkspace(root, task) {
  const files = task.setup(root) || {};
  for (const [path, content] of Object.entries(files)) {
    const abs = join(root, path);
    mkdirSync(join(abs, '..'), { recursive: true });
    writeFileSync(abs, content, 'utf8');
  }
}
function gitInit(root) {
  spawnSync('git', ['init'], { cwd: root });
  spawnSync('git', ['config', 'user.email', 'bench@nc-tools.local'], { cwd: root });
  spawnSync('git', ['config', 'user.name', 'nc-tools bench']);
}

mkdirSync(outDir, { recursive: true });
const summary = [];

for (const model of models) {
  const chat = makeChat({ baseURL, apiKey, model });
  for (const task of tasks) {
    if (onlyTask && task.id !== onlyTask) continue;
    if (onlyTasks.length && !onlyTasks.includes(task.id)) continue;
    for (const mode of ['kernel', 'bash']) {
      for (let rep = 1; rep <= repeatRuns; rep++) {
      const root = mkdtempSync(join(tmpdir(), `nccmp-${task.id}-${mode}-r${rep}-`));
      gitInit(root);
      setupWorkspace(root, task);
      const kernel = new Kernel(root);
      const started = Date.now();
      let result;
      try {
        result = await runAgent({ chat, kernel, task: task.instruction, maxSteps: 40, mode, log: (m) => console.error(m) });
      } catch (e) {
        result = { finalText: `harness error: ${e.message}`, steps: 0, toolCalls: 0, errors: 0, usage: {}, stopped: 'api_error' };
      }
      const wallMs = Date.now() - started;
      let verdict;
      try {
        verdict = await task.verify(root, kernel, result.finalText, mode);
      } catch (e) {
        verdict = { pass: false, evidence: `verifier error: ${e.message}` };
      }
      const journal = kernel.journal.readAll();
      const record = {
        task: task.id, category: task.category, model, mode, repeat: rep,
        solved: verdict.pass === true,
        evidence: String(verdict.evidence || '').slice(0, 600),
        stopped: result.stopped,
        toolCalls: result.toolCalls, toolErrors: result.errors,
        promptTokens: result.usage.prompt_tokens ?? null,
        completionTokens: result.usage.completion_tokens ?? null,
        totalTokens: result.usage.total_tokens ?? null,
        wallMs,
        behavior: behaviorMetrics(journal),
        finalText: String(result.finalText || '').slice(0, 300),
        ts: new Date().toISOString(),
      };
      const runDir = join(outDir, `${model.replaceAll(/[/:]/g, '_')}`);
      mkdirSync(runDir, { recursive: true });
      writeFileSync(join(runDir, `${task.id}.${mode}.json`), JSON.stringify(record, null, 2), 'utf8');
      writeFileSync(join(runDir, `${task.id}.${mode}.journal.jsonl`), journal.map((e) => JSON.stringify(e)).join('\n') + '\n', 'utf8');
      // Windows: a fresh background server may still be starting when the run ends;
      // let the OS finish releasing handles before we delete the workspace.
      await new Promise((r) => setTimeout(r, 1200));
      try {
        rmSync(root, { recursive: true, force: true });
      } catch (e) {
        process.stderr.write(`warn: could not remove ${root}: ${e.message}\n`);
      }
      summary.push(record);
      console.log(`[${model}/${mode}] ${task.id} #${rep}: ${record.solved ? 'SOLVED' : 'FAILED'} (${result.toolCalls} calls, ${record.totalTokens ?? '?'} tok, ${(wallMs / 1000).toFixed(1)}s)`);
      }
    }
  }
}

writeFileSync(join(outDir, 'summary.json'), JSON.stringify(summary, null, 2), 'utf8');

console.log('\n=== HEAD-TO-HEAD SUMMARY (kernel tools vs bash-only) ===');
const agg = {};
for (const r of summary) {
  const k = `${r.model}|${r.mode}`;
  agg[k] ??= { solved: 0, n: 0, calls: 0, errors: 0, tokens: 0, ms: 0 };
  agg[k].n += 1; agg[k].solved += r.solved ? 1 : 0;
  agg[k].calls += r.toolCalls; agg[k].errors += r.toolErrors;
  agg[k].tokens += r.totalTokens ?? 0; agg[k].ms += r.wallMs;
}
for (const [k, v] of Object.entries(agg)) {
  const [model, mode] = k.split('|');
  console.log(`${model} [${mode}]: ${v.solved}/${v.n} solved | ${v.calls} calls (${v.errors} errored) | ~${v.tokens} tok | ${(v.ms / 1000).toFixed(1)}s total`);
}
