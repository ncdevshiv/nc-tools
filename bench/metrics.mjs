// Journal-derived behavior metrics. These are the numbers only a journaled,
// typed tool surface can compute — Terminal-Bench cannot produce them.
// All functions take the parsed journal (array of events) of one run.

/**
 * Wasted-call analysis: calls that errored, or exactly repeated a previous
 * successful call with identical args (redundant work).
 */
export function wastedCalls(journal) {
  const results = new Map(); // callSeq -> result event
  for (const e of journal) if (e.kind === 'tool.result' && e.callSeq != null) results.set(e.callSeq, e);
  const calls = journal.filter((e) => e.kind === 'tool.call' && e.tool !== 'batch.execute');
  const seen = new Map(); // tool + args json -> count
  let errored = 0;
  let redundant = 0;
  for (const c of calls) {
    const r = results.get(c.seq);
    if (r && !r.ok) errored += 1;
    const key = c.tool + '|' + JSON.stringify(c.args ?? {});
    const n = seen.get(key) ?? 0;
    if (n > 0) redundant += 1;
    seen.set(key, n + 1);
  }
  const total = calls.length;
  return {
    totalCalls: total,
    errored,
    redundant,
    wastedRatio: total === 0 ? 0 : (errored + redundant) / total,
  };
}

/**
 * Recovery analysis: for each failed call, how many calls until the next
 * successful call to the SAME tool (the agent fixing its mistake)?
 */
export function recovery(journal) {
  const events = journal.filter((e) => e.kind === 'tool.result' && e.tool !== 'batch.execute');
  const distances = [];
  for (let i = 0; i < events.length; i++) {
    if (events[i].ok) continue;
    const tool = events[i].tool;
    let d = 0;
    for (let j = i + 1; j < events.length; j++) {
      d += 1;
      if (events[j].tool === tool && events[j].ok) { distances.push(d); break; }
    }
    // never recovered with the same tool — count as unresolved
    if (distances.length === 0 || distances[distances.length - 1] !== d) distances.push(Infinity);
  }
  const resolved = distances.filter((d) => d !== Infinity);
  return {
    errorEvents: distances.length,
    resolved: resolved.length,
    medianStepsToRecover: resolved.length === 0
      ? null
      : resolved.sort((a, b) => a - b)[Math.floor(resolved.length / 2)],
  };
}

/** Tool-usage histogram (what the agent actually leaned on). */
export function toolHistogram(journal) {
  const h = {};
  for (const e of journal) {
    if (e.kind !== 'tool.call') continue;
    const base = e.tool.split('.')[0];
    h[base] = (h[base] ?? 0) + 1;
  }
  return h;
}

/** Full metric pack for a run record. */
export function behaviorMetrics(journal) {
  return {
    ...wastedCalls(journal),
    recovery: recovery(journal),
    tools: toolHistogram(journal),
  };
}
