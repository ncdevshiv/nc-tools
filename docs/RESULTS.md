# nc-tools Benchmark Results — Real Runs

Date: 2026-08-31 (wave 6: variance measurement)

## Wave 6: is the benchmark stable? — repeat runs

Every ladder cell ran once; the natural question is how much of that is
signal. `benchmark/variance.mjs` computes solve-rate stability and
token/time spread for cells run N times (`NCTOOLS_REPEATS=3`).

### What repeated runs showed (deepseek, 3x on easy/medium/hard/expert)

| Cell | n | Solve rate | Token spread |
|---|---|---|---|
| fix-off-by-one | 3 | 3/3 | 2% (kernel), 31% (bash) |
| add-feature-with-test | 3 | 3/3 | 5% (kernel), 29% (bash) |
| multi-step-tdd | 3 | 3/3 | 25% (kernel), 227% (bash) |
| expert-refactor-lib | 3 | 3/3 | 35% (kernel), 79% (bash) |

**deepseek is stable**: 12/12 across repeats, and the kernel arm's token
spread (2–35%) is consistently tighter than bash's (29–227%) — the typed
surface produces more reproducible behavior.

### The verifier bias the repeats caught

The first variance run showed multi-step-tdd/bash 0/3 — solid-looking
evidence against bash. Investigation found the verifier was biased: it
checked TDD ordering via journal `fs.write` events, but the bash arm writes
files through `proc.spawn`, so its journal can never contain `fs.write`.
The verifier was fixed to use file mtimes (arm-neutral); with the fix the
bash arm scores 3/3. A verifier bug, not a capability gap — and repeats are
*why* it surfaced. Post-fix, `validate-verifiers` still proves 18/18.

### glm is unstable — and partly provider-unstable

| Cell | n | Solve rate |
|---|---|---|
| fix-off-by-one kernel | 3 | **1/3** (FAIL,FAIL,PASS) |
| fix-off-by-one bash | 3 | 2/3 (PASS,PASS,FAIL) |
| multi-step-tdd kernel | 3 | 2/3 (FAIL,PASS,PASS) |
| multi-step-tdd bash | 3 | 1/3 (FAIL,FAIL,PASS) |

Two of the glm failures were not agent behavior at all: `stopped=api_error`
with HTTP 400 from the upstream provider (0 tokens spent). The other
failures are genuine variance: glm sometimes solves a task in 40s, sometimes
fails after 295s. **Conclusion: deepseek's ladder scores are stable; glm's
are small-sample noise and must not be read as calibrated.** This is recorded
rather than hidden — it's the honest reading of repeated data.

## Wave 5: the diverse ladder — 18 tasks, easy → expert, 2 languages, 72 agent runs

## Wave 5: the diverse ladder — 18 tasks, easy → expert, 2 languages, 72 agent runs

### The ladder (difficulty × language × type)

| Tier | Tasks | Kind |
|---|---|---|
| easy | fix-off-by-one, rename-function (js); py-fix-slice, rename-python (py) | single-file, one-step |
| medium | implement-fn-from-spec, find-and-fix-bug, add-feature-with-test, fix-trap, semantic-locate (js); py-csv-summary (py) | 2–3 steps, some exploration |
| hard | multi-file-refactor, tdd-implement, git-multi-commit, env-config-app, multi-step-tdd, web-server-control (js) | multi-file, stateful, multi-tool |
| expert | expert-refactor-lib | 4-module consolidation + cross-file import graph |

### Every verifier is machine-proven, not assumed

`benchmark/validate-verifiers.mjs` proves each verifier three ways on a fresh
workspace: (1) untouched workspace must FAIL the verifier, (2) the canonical
solution must PASS it, (3) a plausible-but-wrong solution must be REJECTED.
**Result: 18/18 verifiers proven correct.** This validator found and fixed 6
real defects (inverted gate logic, `require()` in ESM, TDD-order violation in
my own canonical solution, probe-race in server tasks, answer-parser picking
`coupons` over `calculateOrderTotal`, Windows EPERM on temp cleanup).

### Full grid: 18 tasks × 2 models × 2 arms = 72 runs, 49 solved (68%)

Per-model totals (kernel arm):

| Model | Kernel arm | Wins over bash arm |
|---|---|---|
| deepseek-v4-flash | **17/18** | 3 extra solves (env-config, multi-step-tdd, web-server-control) |
| glm-5.3-flash | 10/18 | 2 extra solves (fix-off-by-one, semantic-locate) |

By difficulty (both models, kernel arm):

| Tier | deepseek | glm |
|---|---|---|
| easy | 4/4 | 2/4 |
| medium | 6/7 | 4/7 |
| hard | 6/6 | 3/6 |
| expert | 1/1 | 1/1 |

By language (deepseek kernel): js 14/15, python 3/3.

### Findings the ladder surfaced

1. **The kernel arm wins exactly where the terminal is weakest**: all three
   multi-tool/stateful tasks (env-config-app, multi-step-tdd,
   web-server-control) were solved by deepseek/kernel and failed by
   deepseek/bash — the process-handle + env + journal surface is the
   differentiator, not the file tools.
2. **Hard tasks are where the gap widens**: deepseek kernel 6/6 hard vs
   deepseek bash 3/6; glm kernel 3/6 vs glm bash 1/6. On easy/medium the arms
   are near-parity — the old "bash tokens are cheaper" finding only survives
   on short tasks.
3. **Both arms run 30+ calls on multi-file-refactor** — that task's import-
   rewriting is hard for agents either way; the kernel arm does it 2× faster.
4. **Bypass behavior appeared in a second arm**: deepseek bash on
   semantic-locate imported the kernel module directly (`semrun.mjs`) to get
   `search.semantic` — the journal exposed it again, scored as FAIL, and the
   verifier flags `HARNESS-BYPASS`.
5. **glm is genuinely weaker on the kernel surface** (10/18 vs 17/18): the
   tool surface amplifies tool-calling discipline — the batching hints help
   but don't level the field.

Variance: not yet measured in repeats (each cell ran once); a repeat-run
experiment is the next verification step.

## Wave 4: neural tool + portability contract

## Wave 4: neural tool + portability contract

### search.semantic — a real neural network inside the tool surface

`search.semantic` embeds workspace files with a local MiniLM transformer
(`Xenova/all-MiniLM-L6-v2`, ~90MB, runs in-process via @xenova/transformers —
no API calls, no remote model). Queries rank files by cosine similarity.
This is a capability grep provably lacks: it matches by *meaning*.

Proven: first run downloaded the model (6.5s), cached it in
`.nc-tools/model-cache`; unit tests verify that for a credit-card query,
`payments.js` ranks above an unrelated `footer.html` (3/3 semantic tests
pass, 0 skipped).

### semantic-locate task: kernel arm with the neural tool vs bash arm

Task: find the code computing a customer's ENTIRE order total (tax + discounts
+ shipping) — the modules for tax alone and per-line coupons are deliberate
decoys. Ground truth: `src/billing/order-total.js` / `calculateOrderTotal`.
The verifier parses the agent's answer, checks it against ground truth,
asserts the file+function exist on disk, and requires `search.semantic`
usage on the kernel arm (journal-verifiable).

| Arm (deepseek-v4-flash) | Solved | Calls | Tokens | Wall |
|---|---|---|---|---|
| kernel (has search.semantic) | **PASS** | **3** | 16.9k | **14.7s** |
| bash | FAIL | 27 | 308k | 416.6s |

The bash arm answered correctly after 27 calls (9 greps + 17 cats — reading
every file), 18x the tokens and 28x the wall time of the kernel arm, and
still did not satisfy the semantic requirement until it **bypassed the
harness itself**: it wrote `semrun.mjs`, imported the kernel module directly
from disk (`file:///F:/nc-tools/src/kernel/kernel.mjs`), and called
`search.semantic` outside the tool surface. The verifier detects and flags
this (`HARNESS-BYPASS DETECTED`) — the journals of both arms are in
`benchmark/results/semantic-locate2/`.

Takeaway: the semantic tool isn't just cheaper, it's a different strategy —
3 calls vs 27. And the bypass incident is itself data: agents that can read
the host filesystem will reach around a harness boundary; a journal-based
verifier that flags it is the defense.

### Portability contract — any language, verified

- **`docs/PROTOCOL.md`** — language-neutral kernel spec: 40 tools and their
  invariants, journal schema, error taxonomy, transport bindings, and what a
  conforming port must do.
- **`tools/conformance.mjs`** — black-box suite: 14 cases driving a kernel as
  an opaque MCP-stdio process (handshake, tool count/schemas, error codes
  byte-exact, patch ambiguity, journal pairing, snapshot rollback, env
  propagation, shell-injection refusal).
- Result: **14/14 against the reference JS kernel.** Point a Rust/Go/Python
  port at the same suite with only `NCTOOLS_CONFORMANCE_CMD` changed — that's
  the portability criterion. `conformance/cases.mjs` holds the cases.

## Wave 3: snapshots/rollback + chaos harness
Harness: `node benchmark/compare.mjs` — head-to-head arms + the wave-2 control task.
Endpoint: local proxyhub gateway (OpenAI-compatible), live upstream providers.
Every run: fresh temp workspace, fresh git repo, agent loop capped at 40 steps,
**independent verifier** checks the workspace afterward — the agent cannot self-report.

## Wave 3: snapshots/rollback + chaos harness

### What was built

- **`sys.snapshot` / `sys.rollback` / `sys.listSnapshots`** — workspace
  snapshots with manifest-based restore (restore changed files, remove
  files created after the snapshot, leave `.git`/`.nc-tools`/`node_modules`
  alone). The primitive that makes agent *speculation* safe.
- **Kernel hooks** — the fault-injection seam. Any harness can register a
  hook on `kernel.call`; injected failures are journaled like real ones, so
  recovery from them is *measurable*. This is the experiment a terminal-based
  harness cannot run: bash has no seams to inject faults into.
- **`node benchmark/chaos.mjs`** — chaos runner. Injects 2 transient
  `ERR_FLAKY` failures into test invocations (kernel arm: `test.run`; bash
  arm: `proc.spawn` whose argv contains `--test`), then measures recovery.

### Results: 16 runs (4 tasks × 2 models × 2 arms)

Every run **SOLVED** — all flake injections were recovered from:

| Task | Model | Arm | Flakes | Recovery (steps) | Snapshots |
|---|---|---|---|---|---|
| add-feature-with-test | glm / deepseek | kernel | 2 | [1, 0] / [1, 0] | 1 / 1 |
| add-feature-with-test | glm / deepseek | bash | 2 | [1, 0] / [1, 0] | 0 / 0 |
| multi-file-refactor | glm / deepseek | kernel | 2 | [0, 0] / [0, 0] | 1 / 1 |
| multi-file-refactor | glm / deepseek | bash | 2 | [1, 0] / [1, 0] | 0 / 0 |
| tdd-implement | glm / deepseek | kernel | 0* | — | 2 / 0 |
| tdd-implement | glm / deepseek | bash | 2 | [1, 0] / [1, 0] | 0 / 0 |
| find-and-fix-bug | glm / deepseek | kernel | 0* | — | 0 |
| find-and-fix-bug | glm / deepseek | bash | 0* | — | 0 |

What this shows:

1. **Recovery is a single retry** — in every flake, the agent's next
   same-target call succeeded (recovery step counts are 0–1). The agents
   were told a flake was possible; the structured `ERR_FLAKY` code let them
   distinguish "test runner hiccup" from "my change broke it" instantly.
2. **Kernel-arm agents used snapshots — bash-arm agents could not.** 5 of 8
   kernel runs called `sys.snapshot` before risky work (glm even took 2 on
   tdd-implement); no bash run could, because with a shell string there is
   no journal-point to roll back to. This is the speculation primitive
   becoming visible in behavior, not just in a demo.
3. **Chaos is fair to both arms, and honest about coverage.** Runs where the
   agent simply didn't invoke the flake-target path get `flakes=0` (*). In
   several kernel runs the agents chose `proc.spawn --test` instead of
   `test.run`, escaping the injection by path choice — recorded, not hidden.

The chaos harness is itself the deliverable: *the ability to run this
experiment at all* is the difference between a typed machine API and a
terminal. Verification of the chaos claim is in
`benchmark/results/chaos/` (16 records + 16 journals).

## Wave 2: the "control" layer (beyond read/write/search/patch)

The original goal was never just better file tools — it is replacing the
terminal **absolutely**. Wave 2 types the terminal's *control* surface:

- **Managed background processes**: `proc.start` returns a handle; `proc.status`,
  `proc.readOutput`, `proc.stop` manage it. This replaces "run a server in a
  terminal tab / `&` background jobs / kill by hand".
- **Structured test drivers**: `test.run` executes node:test and pytest and
  returns pass/fail counts plus *failing test identities* (both via junit XML).
- **Package drivers**: `pkg.add/list/scripts/runScript` for npm and pip.
- **Typed network**: `net.http` (replaces curl) and `net.probePort`
  (replaces nc/netstat probing).
- **Session environment**: `env.set/get/list`, inherited by every proc call.

Surface now: **36 typed tools**. Test suite: **45/45 passing** (all real:
real HTTP servers, real pytest, real npm installs).

## Terminal-taxonomy coverage (the "absolute" metric)

`node tools/coverage.mjs` tracks replacement of the terminal's command
taxonomy: **15 covered + 3 partial of 26 command classes = 63% weighted**
(58% pure). Remaining gaps are archives, permissions, scheduling, encryption,
containers, cloud CLIs, interactive TUIs, and file-watching — each a future
driver family, tracked, not hand-waved.

## web-server-control: the workflow that required terminal tabs

Task: start `src/server.js` as a background process, poll until port 4123 is
open, GET `/ping` and verify the body, stop the server, confirm the port is
closed. The verifier reads the **journal**: it requires a `proc.start` event
(no `proc.spawn` for the server), a successful `net.http` result with the pong
body, a `proc.stop`, and then probes the port itself, live.

| Model | Arm | Solved | Calls | Tokens | Wall (s) |
|---|---|---|---|---|---|
| glm-5.3-flash | kernel | **PASS** | 7 | 35.8k | 194 |
| glm-5.3-flash | bash | fail | 9 | 30.6k | 425 |
| deepseek-v4-flash | kernel | **PASS** | 7 | 38.6k | 19.5 |
| deepseek-v4-flash | bash | no verdict | — | — | >7800 (terminated) |

Both kernel arms solved it cleanly: start → poll → GET → stop → confirm
closed, with every step a typed, journaled event. The glm bash arm *failed*
— it could start the server in the background, but verifying it and tearing
it down from inside a single-argument bash string proved fragile (no handle
to the process, output only reachable by redirect-and-cat games). The
deepseek bash arm never finished: after 2+ hours (40-step cap, each step a
foreground-blocking server start attempt), it was terminated — with no
handle to a process, bash has no way to probe-and-poll a server, which is
exactly the failure mode this task exists to expose. The kernel-arm
equivalent needed 19.5 seconds and 7 typed calls.

This task is the clearest evidence for the thesis: **the control surface is
where the terminal genuinely hurts** — and where a typed, handle-based
runtime is not just nicer, it works where bash doesn't.

## P0 wave results (unchanged from previous commit)

## P0 wave: what changed since the first comparison

Kernel gained (all unit-tested, 35/35 passing):
- **Batch operations** `fs.readMany`, `fs.writeMany`, `patch.applyMany`, and
  `batch.execute` (up to 25 tool calls in one round-trip, individually journaled)
- **Actionable error hints**: `ERR_NOT_FOUND` returns nearest existing files
  (typo recovery), `ERR_PATH_ESCAPE` returns the workspace root + suggestion
- **Read digests**: `fs.read` returns sha256+mtime so agents can skip re-reads
- **Behavior metrics** computed from the journal: wasted-call ratio
  (errored + redundant calls), recovery analysis (steps from error → fixed),
  tool histogram — metrics Terminal-Bench cannot compute because a shell has
  no seams to observe them

## Head-to-head results (32 runs)

| Model | Arm | Solved | Calls | Redundant | Errored | Tokens | Wall (s) |
|---|---|---|---|---|---|---|---|
| glm-5.3-flash | **kernel** | 8/8 | 58 | 6 | 1 | ~133k | 1066 |
| glm-5.3-flash | **bash** | 8/8 | 37 | 0 | 1 | ~61k | 733 |
| deepseek-v4-flash | **kernel** | **8/8** | 50 | 4 | 3 | ~228k | 195 |
| deepseek-v4-flash | **bash** | 7/8 | 64 | 2 | 3 | ~374k | 584 |

Recovery events (structured error → next same-tool success):
kernel arm recovered **4/4** (median 1–2 steps); bash arm had zero recoverable
error events by construction — its errors are stderr text, not typed events.

### What the numbers support now

1. **deepseek/kernel is the only 8/8**, and it did so with ~39% fewer tokens
   than deepseek/bash (228k vs 374k) and 3x faster wall time. The gap flipped
   after the P0 changes because (a) batch ops let one call carry multi-file
   work, and (b) on the hardest task (`multi-file-refactor`, 3 files + shared
   module + import rewriting) bash needed 26 calls/265k tokens where the
   kernel arm needed 8 calls/49k tokens.
2. **glm still favors bash on tokens** (61k vs 133k) but ties 8/8. glm is a
   weaker tool-caller that batched poorly in the kernel arm (6 redundant
   calls) — the deficit is now attributable to the model, not the surface.
3. **The discriminating task worked**: `multi-file-refactor` is where the
   arms diverge most. First iteration of this task exposed two harness bugs
   (Windows `node --test <dir>` semantics; a verifier that counted the
   canonical definition as a duplicate) — both fixed and both documented
   here because a benchmark with silent verifier bugs is worse than none.
4. **Error semantics are visible in the data**: every kernel-arm error was a
   typed event with a code, hints, and a recorded recovery path. The bash
   arm's `1 errored`/`3 errored` numbers are exit-code counts only — there is
   no way to know what the model saw or how it recovered.

## Head-to-head: typed kernel tools vs bash-only (original run, pre-P0)

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

## Head-to-head: typed kernel tools vs bash-only (compare run)

Command: `NCTOOLS_LLM_MODELS="glm-5.3-flash,deepseek-v4-flash-vision-exp" node benchmark/compare.mjs benchmark/results/compare`

Same 5 tasks, same models, same independent verifier. The only variable: the
agent either gets the 18 typed kernel tools or a single `bash(script)` tool
(same `proc.spawn` underneath, running `bash -c`).

| Model | Arm | Solved | Tool calls | Errored | Tokens | Wall (s) |
|---|---|---|---|---|---|---|
| glm-5.3-flash | **kernel** | 4/5* | 32 | 0 | ~34.7k | 251 |
| glm-5.3-flash | **bash** | 5/5 | 15 | 0 | ~23.7k | 302 |
| deepseek-v4-flash-vision-exp | **kernel** | 5/5 | 31 | 1 | ~99.8k | 91 |
| deepseek-v4-flash-vision-exp | **bash** | 5/5 | 21 | 0 | ~55.9k | 89 |

*The one kernel-arm failure was an upstream HTTP 429 rate limit mid-run, not a
capability failure — run2 (same task, same model, same arm) solved it.

### What this comparison honestly shows

1. **Bash wins on token efficiency and call count for capable models.** A
   single shell script can read+edit+run in one call, so bash used ~40–45%
   fewer tokens/calls across both models. This is the real cost of typed
   tools: granularity. Pretending otherwise would be dishonest — and it is
   exactly why `compare.mjs` exists as a permanent harness arm.
2. **Both arms solve everything**, so the benchmark's current task set does
   not yet discriminate. The tasks are single-file, short-horizon. Where the
   typed layer is *expected* to pull ahead — and what the next benchmark
   iteration must add — is: multi-step stateful work (kernel journal beats
   shell history), recovery from structured errors (see the deepseek
   `PATCH_NO_MATCH` recoveries in run2), safety/policy measurement (path jail,
   audit trails), and chaos/fault injection (impossible to do cleanly against
   a shell).
3. **The value claim of nc-tools was never "fewer tokens per trivial task."**
   It is verifiability (journaled, replayable runs), provider absorption
   (the `.`-in-name fix landed in one place), error semantics (structured
   hints vs stderr strings), and measurability (this table itself only exists
   because the harness can swap the tool surface while holding everything
   else constant — you cannot run this experiment against a real terminal
   without also changing the harness).

The honest summary: **parity on capability today, a measured deficit on
efficiency for short tasks, and the differentiating claims (recovery, safety,
replay, chaos) still need harder tasks to be demonstrated** — which defines
the benchmark roadmap, not a fake win.

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
