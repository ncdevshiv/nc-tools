// Chaos harness: injects transient driver failures the way the real world
// produces them (flaky CI, timeouts, network blips) and measures recovery.
// This is the experiment a terminal-based harness CANNOT run: bash has no
// seams to break — the kernel's hooks are exactly where faults live.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { Kernel } from '../src/kernel/kernel.mjs';
import { runAgent } from '../src/agent/agent.mjs';
import { makeChat } from '../src/agent/llm.mjs';
import { tasks } from './tasks.mjs';
import { behaviorMetrics } from './metrics.mjs';

const baseURL = process.env.NCTOOLS_LLM_BASEURL;
const apiKey = process.env.NCTOOLS_LLM_APIKEY || '';
const models = (process.env.NCTOOLS_LLM_MODELS || '').split(',').map((s) => s.trim()).filter(Boolean);
const outDir = resolve(process.argv[2] || 'benchmark/results/chaos');

if (!baseURL || models.length === 0) {
  console.error('Usage: NCTOOLS_LLM_BASEURL=... NCTOOLS_LLM_APIKEY=... NCTOOLS_LLM_MODELS=a node benchmark/chaos.mjs [outDir]');
  process.exit(2);
}

// Chaos profile: the first N relevant calls fail with a structured transient
// error, then the driver works normally. Exactly like flaky CI.
const FLAKE_PROFILE = {
  flakeFirstN: 2,
  error: () => ({
    code: 'ERR_FLAKY',
    message: 'transient failure: test runner lost its workspace lock (CI-flake simulation)',
    hint: { retryRecommended: true },
  }),
};

function installChaos(kernel, mode) {
  let flakeCount = 0;
  kernel.hooks.push((tool, args) => {
    const isFlakyTarget = mode === 'kernel' ? tool === 'test.run' : (tool === 'proc.spawn' && JSON.stringify(args?.args ?? []).includes('--test'));
    if (!isFlakyTarget) return null;
    flakeCount += 1;
    if (flakeCount <= FLAKE_PROFILE.flakeFirstN) return FLAKE_PROFILE.error();
    return null;
  });
  return kernel;
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

const chaosTasks = tasks.filter((t) => t.id === 'tdd-implement' || t.id === 'multi-file-refactor' || t.id === 'add-feature-with-test' || t.id === 'find-and-fix-bug');

mkdirSync(outDir, { recursive: true });
const summary = [];

for (const model of models) {
  const chat = makeChat({ baseURL, apiKey, model });
  for (const task of chaosTasks) {
    for (const mode of ['kernel', 'bash']) {
      const root = mkdtempSync(join(tmpdir(), `ncchaos-${task.id}-${mode}-`));
      gitInit(root);
      setupWorkspace(root, task);
      const kernel = installChaos(new Kernel(root), mode);
      const started = Date.now();
      let result;
      try {
        result = await runAgent({
          chat, kernel, task: task.instruction + '\n\nNOTE: the test runner may fail transiently once or twice (CI flakes). If a test-run call fails and you have NOT changed code since the last successful state, retry the same call before assuming your change broke things, and use a snapshot (sys.snapshot) before risky edits so you can sys.rollback if needed.',
          maxSteps: 40, mode, log: (m) => console.error(m),
        });
      } catch (e) {
        result = { finalText: `harness error: ${e.message}`, steps: 0, toolCalls: 0, errors: 0, usage: {}, stopped: 'api_error' };
      }
      const wallMs = Date.now() - started;
      let verdict;
      try { verdict = await task.verify(root, kernel, result.finalText, mode); } catch (e) { verdict = { pass: false, evidence: `verifier error: ${e.message}` }; }
      const journal = kernel.journal.readAll();
      const flakeEvents = journal.filter((e) => e.kind === 'tool.result' && e.error?.code === 'ERR_FLAKY');
      // recovery: after each flake, steps until next successful same-target call
      const results = journal.filter((e) => e.kind === 'tool.result');
      const recoverySteps = [];
      for (const f of flakeEvents) {
        const idx = results.indexOf(f);
        for (let j = idx + 1; j < results.length; j++) {
          if (results[j].ok) { recoverySteps.push(j - idx - 1); break; }
        }
      }
      // did the agent use snapshot/rollback?
      const snapshotCalls = journal.filter((e) => e.kind === 'tool.call' && e.tool === 'sys.snapshot').length;
      const rollbackCalls = journal.filter((e) => e.kind === 'tool.call' && e.tool === 'sys.rollback').length;
      const record = {
        task: task.id, model, mode,
        solved: verdict.pass === true,
        evidence: String(verdict.evidence || '').slice(0, 500),
        stopped: result.stopped,
        toolCalls: result.toolCalls, toolErrors: result.errors,
        totalTokens: result.usage.total_tokens ?? null,
        promptTokens: result.usage.prompt_tokens ?? null,
        wallMs,
        flakesInjected: flakeEvents.length,
        recoverySteps,
        maxRecoverySteps: recoverySteps.length ? Math.max(...recoverySteps) : 0,
        recoveredAllFlakes: recoverySteps.length === flakeEvents.length,
        usedSnapshot: snapshotCalls, usedRollback: rollbackCalls,
        behavior: behaviorMetrics(journal),
        ts: new Date().toISOString(),
      };
      const runDir = join(outDir, model.replaceAll(/[/:]/g, '_'));
      mkdirSync(runDir, { recursive: true });
      writeFileSync(join(runDir, `${task.id}.${mode}.json`), JSON.stringify(record, null, 2), 'utf8');
      writeFileSync(join(runDir, `${task.id}.${mode}.journal.jsonl`), journal.map((e) => JSON.stringify(e)).join('\n') + '\n', 'utf8');
      rmSync(root, { recursive: true, force: true });
      summary.push(record);
      console.log(`[${model}/${mode}] ${task.id}: ${record.solved ? 'SOLVED' : 'FAILED'} flakes=${record.flakesInjected} recovery=${JSON.stringify(record.recoverySteps)} snaps=${record.usedSnapshot}/${record.usedRollback} (${record.toolCalls} calls, ${(wallMs / 1000).toFixed(0)}s)`);
    }
  }
}

writeFileSync(join(outDir, 'summary.json'), JSON.stringify(summary, null, 2), 'utf8');
console.log('\n=== CHAOS SUMMARY ===');
for (const r of summary) {
  console.log(`${r.model} [${r.mode}] ${r.task}: ${r.solved ? 'PASS' : 'FAIL'} | flakes=${r.flakesInjected} recovered=${r.recoveredAllFlakes} (${JSON.stringify(r.recoverySteps)}) | snaps=${r.usedSnapshot}/${r.usedRollback}`);
}
