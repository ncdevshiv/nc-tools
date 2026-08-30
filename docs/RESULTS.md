# nc-tools Benchmark Results — Real Runs

Date: 2026-08-30
Harness: `node benchmark/harness.mjs benchmark/results/run2`
Endpoint: local proxyhub gateway (OpenAI-compatible), live upstream providers.
Every run: fresh temp workspace, fresh git repo, agent loop capped at 40 steps,
**independent verifier** checks the workspace afterward (real `node --test`
runs, real behavior checks via `proc.spawn`) — the agent cannot self-report.

## Command to reproduce

```bash
set -a; source .env.bench; set +a
NCTOOLS_LLM_MODELS="glm-5.3-flash,deepseek-v4-flash-vision-exp" \
  node benchmark/harness.mjs benchmark/results/run2
```

## Per-run results (run2)

| Model | Task | Solved | Tool calls | Tool errors | Tokens | Wall (s) |
|---|---|---|---|---|---|---|
| glm-5.3-flash | fix-off-by-one | PASS | 3 | 0 | 3,454 | 39.6 |
| glm-5.3-flash | rename-function | PASS | 8 | 0 | 12,967 | 39.9 |
| glm-5.3-flash | implement-fn-from-spec | PASS | 8 | 0 | 31,425 | 98.9 |
| glm-5.3-flash | find-and-fix-bug | PASS | 6 | 0 | 8,207 | 41.5 |
| glm-5.3-flash | add-feature-with-test | PASS | 6 | 0 | 6,336 | 61.1 |
| deepseek-v4-flash-vision-exp | fix-off-by-one | PASS | 4 | 0 | 15,819 | 18.1 |
| deepseek-v4-flash-vision-exp | rename-function | PASS | 9 | 0 | 17,793 | 21.0 |
| deepseek-v4-flash-vision-exp | implement-fn-from-spec | PASS | 8 | 1 | 22,806 | 33.4 |
| deepseek-v4-flash-vision-exp | find-and-fix-bug | PASS | 10 | 1 | 31,611 | 23.3 |
| deepseek-v4-flash-vision-exp | add-feature-with-test | PASS | 5 | 0 | 17,958 | 16.0 |

**Model totals:**
- glm-5.3-flash: **5/5 solved**, 31 tool calls, 0 errored, ~62k tokens
- deepseek-v4-flash-vision-exp: **5/5 solved**, 36 tool calls, 2 errored, ~106k tokens

Efficiency signal: glm used ~40% fewer tokens than deepseek for the same
10/10 outcome; deepseek was ~2x faster in wall time. Both operated fully
terminal-free.

## What the tasks were

- `fix-off-by-one` — fix a wrong `slice` in existing code (verified by executing it)
- `rename-function` — rename across 2 files incl. import sites (verified by grep: 0 old-name hits, plus behavior check)
- `implement-fn-from-spec` — write `debounce` + a passing `node:test` suite from scratch
- `find-and-fix-bug` — diagnose a JSON syntax error in a config file breaking `node src/app.js` (verified by exact program output)
- `add-feature-with-test` — extend a Stack class with `peek`/`isEmpty` + tests (verified by full test run)

## Artifacts on disk

- `benchmark/results/run2/summary.json` — machine-readable summary of all 10 runs
- `benchmark/results/run2/<model>/<task>.json` — per-run record (tokens, calls, verdict, evidence)
- `benchmark/results/run2/<model>/<task>.journal.jsonl` — the full kernel event journal: every tool call + structured result, replayable

Sample journal proof (deepseek, `fix-off-by-one`): 5 tool calls, 5 results —
`fs.read` → `patch.apply` (exact-match edit) → `fs.read` (verify) →
`proc.spawn` (node behavior check) → `fs.read`.

## Notes

- Run1 initially scored deepseek 0/5: the upstream rejects function names
  containing `.` (OpenAI spec allows them; some providers don't). Fixed in
  `src/agent/agent.mjs` with wire-name mapping (`fs.read` ↔ `fs__read`).
  This is exactly the class of provider-compat issue the typed layer absorbs
  in one place instead of in every agent.
- The two `toolErrors` on deepseek runs are structured kernel errors the agent
  received mid-run (e.g. a `PATCH_NO_MATCH` with nearest-line hints) and
  recovered from on the next step — the recovery behavior the journal makes
  measurable.
