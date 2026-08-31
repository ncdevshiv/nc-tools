// Benchmark harness: task x model -> fresh workspace -> agent run -> real
// verifier -> transcript JSON with metrics. Every artifact is on disk.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../oracle/kernel/kernel.mjs';
import { runAgent } from '../oracle/agent/agent.mjs';
import { makeChat } from '../oracle/agent/llm.mjs';
import { tasks } from './tasks.mjs';

const baseURL = process.env.NCTOOLS_LLM_BASEURL;
const apiKey = process.env.NCTOOLS_LLM_APIKEY || '';
const models = (process.env.NCTOOLS_LLM_MODELS || '').split(',').map((s) => s.trim()).filter(Boolean);
const outDir = resolve(process.argv[2] || 'benchmark/results');
const onlyTask = process.env.NCTOOLS_TASK;

if (!baseURL || models.length === 0) {
  console.error('Usage: NCTOOLS_LLM_BASEURL=... NCTOOLS_LLM_APIKEY=... NCTOOLS_LLM_MODELS=model-a,model-b node benchmark/harness.mjs [outDir]');
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

// task workspaces need to be git repos for git.* tools to be usable (agent may call them)
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
    const root = mkdtempSync(join(tmpdir(), `ncbench-${task.id}-`));
    gitInit(root);
    setupWorkspace(root, task);
    const kernel = new Kernel(root);
    const started = Date.now();
    let result;
    try {
      result = await runAgent({ chat, kernel, task: task.instruction, maxSteps: 40, log: (m) => console.error(m) });
    } catch (e) {
      result = { finalText: `harness error: ${e.message}`, steps: 0, toolCalls: 0, errors: 0, usage: {}, stopped: 'api_error' };
    }
    const wallMs = Date.now() - started;
    let verdict;
    try {
      verdict = await task.verify(root, kernel, result.finalText, 'kernel');
    } catch (e) {
      verdict = { pass: false, evidence: `verifier error: ${e.message}` };
    }
    const record = {
      task: task.id,
      category: task.category,
      model,
      solved: verdict.pass === true,
      evidence: String(verdict.evidence || '').slice(0, 1000),
      stopped: result.stopped,
      toolCalls: result.toolCalls,
      toolErrors: result.errors,
      promptTokens: result.usage.prompt_tokens ?? null,
      completionTokens: result.usage.completion_tokens ?? null,
      totalTokens: result.usage.total_tokens ?? null,
      steps: result.steps,
      wallMs,
      finalText: String(result.finalText || '').slice(0, 500),
      workspace: root,
      ts: new Date().toISOString(),
    };
    // persist transcript (journal) alongside the record
    const journal = kernel.journal.readAll();
    const runDir = join(outDir, `${model.replaceAll(/[/:]/g, '_')}`);
    mkdirSync(runDir, { recursive: true });
    writeFileSync(join(runDir, `${task.id}.json`), JSON.stringify(record, null, 2), 'utf8');
    writeFileSync(join(runDir, `${task.id}.journal.jsonl`), journal.map((e) => JSON.stringify(e)).join('\n') + '\n', 'utf8');
    rmSync(root, { recursive: true, force: true });
    summary.push(record);
    console.log(`[${model}] ${task.id}: ${record.solved ? 'SOLVED' : 'FAILED'} (${result.toolCalls} calls, ${record.totalTokens ?? '?'} tok, ${(wallMs / 1000).toFixed(1)}s)`);
  }
}

writeFileSync(join(outDir, 'summary.json'), JSON.stringify(summary, null, 2), 'utf8');

// printed table
console.log('\n=== BENCHMARK SUMMARY ===');
for (const r of summary) {
  console.log(`${r.model} | ${r.task} | ${r.solved ? 'PASS' : 'FAIL'} | calls=${r.toolCalls} errors=${r.toolErrors} tok=${r.totalTokens ?? '?'} ${(r.wallMs / 1000).toFixed(1)}s`);
}
const byModel = {};
for (const r of summary) {
  byModel[r.model] ??= { solved: 0, total: 0, tokens: 0, calls: 0, errors: 0 };
  byModel[r.model].total += 1;
  byModel[r.model].solved += r.solved ? 1 : 0;
  byModel[r.model].tokens += r.totalTokens ?? 0;
  byModel[r.model].calls += r.toolCalls;
  byModel[r.model].errors += r.toolErrors;
}
for (const [m, s] of Object.entries(byModel)) {
  console.log(`MODEL ${m}: ${s.solved}/${s.total} solved, ${s.calls} tool calls (${s.errors} errored), ~${s.tokens} tokens`);
}
