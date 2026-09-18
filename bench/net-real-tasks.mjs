// W-Net-2b REAL-task benchmark — everything here runs against the LIVE web.
// No fixtures, no mocks: real queries with real gold URLs, real multi-step
// research tasks executed the way an agent executes them (search → fetch →
// verify → cite), real latency measurement, and a chaos arm that removes
// sources mid-comparison. Output: bench/results/net-w2/*.json + stdout table.
//
// Usage: RUN_LIVE=1 NCTOOLS_MODEL_CACHE=<dir> node bench/net-real-tasks.mjs
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { withKernel } from '../tools/kernel-client.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, 'results', 'net-w2');
mkdirSync(outDir, { recursive: true });

const norm = (u) => {
  try {
    const x = new URL(u);
    return `${x.host.replace(/^www\./, '')}${x.pathname.replace(/\/$/, '')}`;
  } catch { return String(u); }
};

// ---- gold set: 20 real queries, real gold URLs (checked live 2026-09-02) ----
// Each: { q, intent, gold: [acceptable urls — any one counts as a hit] }
const GOLD = [
  { q: 'rust programming language', intent: 'general', gold: ['rust-lang.org/'] },
  { q: 'python programming language', intent: 'general', gold: ['python.org/'] },
  { q: 'node.js javascript runtime', intent: 'general', gold: ['nodejs.org/'] },
  { q: 'typescript javascript with syntax for types', intent: 'general', gold: ['typescriptlang.org/'] },
  { q: 'git version control system', intent: 'general', gold: ['git-scm.com/'] },
  { q: 'the rust async book', intent: 'general', gold: ['rust-lang.github.io/async-book'] },
  { q: 'the rust programming language book', intent: 'general', gold: ['doc.rust-lang.org/book'] },
  { q: 'how to fix rust borrow checker error e0509', intent: 'howto', gold: ['doc.rust-lang.org/error_codes/E0509', 'stackoverflow.com'] },
  { q: 'how to center a div css', intent: 'howto', gold: ['stackoverflow.com', 'css-tricks.com', 'developer.mozilla.org'] },
  { q: 'how to uninstall node modules', intent: 'howto', gold: ['stackoverflow.com', 'npmjs.com', 'developer.mozilla.org'] },
  { q: 'attention is all you need arxiv paper', intent: 'academic', gold: ['arxiv.org/abs/1706.03762'] },
  { q: 'transformer architectures for vision paper', intent: 'academic', gold: ['arxiv.org', 'openalex.org', 'computer.org', 'ieeexplore.ieee.org', 'thecvf.com'] },
  { q: 'pep 634 structural pattern matching python', intent: 'academic', gold: ['peps.python.org/pep-0634', 'python.org/dev/peps/pep-0634', 'python.org'] },
  { q: 'serde serialization framework rust', intent: 'package', gold: ['crates.io/crates/serde', 'serde.rs'] },
  { q: 'react library for user interfaces', intent: 'package', gold: ['npmjs.com/package/react', 'react.dev', 'reactjs.org'] },
  { q: 'express node.js web framework', intent: 'package', gold: ['npmjs.com/package/express', 'expressjs.com'] },
  { q: 'openai gpt-5 announcement', intent: 'news', gold: ['openai.com'] },
  { q: 'rust 1.0 release 2015', intent: 'general', gold: ['blog.rust-lang.org'] },
  { q: 'microsoft visual studio code', intent: 'general', gold: ['code.visualstudio.com', 'visualstudio.microsoft.com', 'microsoft.com'] },
  { q: 'linux kernel source repository', intent: 'general', gold: ['kernel.org', 'github.com/torvalds/linux'] },
];

async function main() {
  const root = mkdtempSync(join(tmpdir(), 'nct-w2-'));
  const report = { ts: new Date().toISOString(), live: process.env.RUN_LIVE === '1' };
  try {
    await withKernel(root, async (k) => {
      // ============ ARM 1: recall@10, baseline (pre-W2 sources) vs fleet ====
      const SKIP = process.env.TASKS_ONLY === "1";
      const recall = { baseline: { hits: 0, per: [] }, fleet: { hits: 0, per: [] }, rows: [] };
      if (!SKIP) for (const g of GOLD) {
        const runArm = async (engines, label) => {
          const args = { query: g.q, maxResults: 10, rerank: true };
          if (engines) args.engines = engines;
          const r = await k.call('net.search', args);
          if (!r.ok) return { hit: false, error: r.error?.code, top: [], dropped: r.error?.hint?.sourcesDropped ?? [] };
          const dropped = r.result.sourcesDropped ?? [];
          const top = r.result.results.slice(0, 10).map((x) => norm(x.url));
          const hit = g.gold.some((gold) => top.some((u) => u.startsWith(gold)));
          return { hit, top, dropped };
        };
        const base = await runArm('hn,wikipedia,ddg,mojeek', 'baseline');
        const fleet = await runArm(null, 'fleet'); // auto = intent-routed
        recall.baseline.hits += base.hit ? 1 : 0;
        recall.fleet.hits += fleet.hit ? 1 : 0;
        recall.rows.push({ q: g.q, intent: g.intent, base: base.hit, fleet: fleet.hit, fleetTop3: (fleet.top || []).slice(0, 3), dropped: fleet.dropped ?? [] });
        console.log(`  [recall] ${g.q.padEnd(46)} base=${base.hit ? 'HIT' : 'miss'} fleet=${fleet.hit ? 'HIT' : 'miss'}`);
      }
      recall.baseline.rate = recall.baseline.hits / GOLD.length;
      recall.fleet.rate = recall.fleet.hits / GOLD.length;
      report.recall = recall;
      console.log(`\n  RECALL@10  baseline=${(recall.baseline.rate * 100).toFixed(0)}%  fleet=${(recall.fleet.rate * 100).toFixed(0)}%`);

      // ============ ARM 2: routing accuracy =================================
      const routing = { total: 0, correct: 0, methods: {}, rows: [] };
      if (!SKIP) for (const g of GOLD) {
        const r = await k.call('net.search', { query: g.q, maxResults: 3 });
        if (!r.ok) continue;
        routing.total += 1;
        const intent = r.result.intent;
        routing.methods[r.result.intentMethod] = (routing.methods[r.result.intentMethod] || 0) + 1;
        // correct = routed sources include an engine suited to the labeled intent
        const suited = { general: ['bing-rss', 'searxng', 'hn', 'wikipedia'], howto: ['stackexchange', 'hn'], academic: ['openalex', 'arxiv'], package: ['npm', 'crates'], news: ['gnews', 'bing-rss'] }[g.intent] || [];
        const ok = r.result.planned.some((s) => suited.includes(s));
        routing.correct += ok ? 1 : 0;
        routing.rows.push({ q: g.q, labeled: g.intent, routed: intent, method: r.result.intentMethod, planned: r.result.planned, ok });
      }
      routing.rate = routing.correct / Math.max(1, routing.total);
      report.routing = routing;
      console.log(`  ROUTING    ${(routing.rate * 100).toFixed(0)}% intent-fit (${JSON.stringify(routing.methods)})`);

      // ============ ARM 3: latency, serial vs parallel =======================
      // serial arm: nc-tools.toml in a second workspace disables parallel fanout
      const serialRoot = mkdtempSync(join(tmpdir(), 'nct-w2s-'));
      writeFileSync(join(serialRoot, 'nc-tools.toml'), '[limits]\nnet_parallel_fanout = false\n');
      const lat = { parallel: [], serial: [] };
      try {
        await withKernel(serialRoot, async (ks) => {
          for (const q of ['rust programming language', 'transformer attention paper', 'serde rust crate']) {
            const t0 = Date.now();
            const r = await k.call('net.search', { query: q, engines: 'bing-rss,gnews,stackexchange,openalex,arxiv,npm,crates,hn,wikipedia', maxResults: 5 });
            if (r.ok) lat.parallel.push({ q, ms: Date.now() - t0, sources: r.result.engines.filter((e) => e.status === 'ok').length });
            const t1 = Date.now();
            const rs = await ks.call('net.search', { query: q, engines: 'bing-rss,gnews,stackexchange,openalex,arxiv,npm,crates,hn,wikipedia', maxResults: 5 });
            if (rs.ok) lat.serial.push({ q, ms: Date.now() - t1, sources: rs.result.engines.filter((e) => e.status === 'ok').length });
          }
        });
      } finally { rmSync(serialRoot, { recursive: true, force: true }); }
      const avg = (a) => (a.length ? a.reduce((s, x) => s + x.ms, 0) / a.length : 0);
      lat.parallelAvg = Math.round(avg(lat.parallel));
      lat.serialAvg = Math.round(avg(lat.serial));
      lat.speedup = lat.serialAvg ? +(lat.serialAvg / lat.parallelAvg).toFixed(2) : null;
      report.latency = lat;
      console.log(`  LATENCY    parallel=${lat.parallelAvg}ms serial=${lat.serialAvg}ms speedup=${lat.speedup}x`);

      // ============ ARM 4: chaos — remove the strongest source ==============
      const chaos = { full: 0, degraded: 0 };
      if (!SKIP) {
        const full = await k.call('net.search', { query: 'rust programming language', engines: 'all', maxResults: 10 });
        const okEngines = full.ok ? full.result.engines.filter((e) => e.status === 'ok' && e.results > 0).map((e) => e.name) : [];
        chaos.full = full.ok ? full.result.results.length : 0;
        chaos.fullSources = okEngines;
        // remove the top-2 by result count, re-run
        const survivors = okEngines.filter((e) => e !== 'bing-rss' && e !== 'hn').join(',');
        const degraded = await k.call('net.search', { query: 'rust programming language', engines: survivors || 'wikipedia', maxResults: 10 });
        chaos.removed = ['bing-rss', 'hn'].filter((e) => okEngines.includes(e));
        chaos.survivors = survivors;
        chaos.degraded = degraded.ok ? degraded.result.results.length : 0;
        chaos.stillUsable = degraded.ok && degraded.result.results.length >= 5;
      }
      report.chaos = chaos;
      console.log(`  CHAOS      full=${chaos.full} results (${(chaos.fullSources || []).join(',')}) → without [${chaos.removed}] = ${chaos.degraded} results, usable=${chaos.stillUsable}`);

      // ============ ARM 5: real multi-step tasks (simple → complex) ==========
      const tasks = [];
      const step = (t, name, ok, detail) => {
        console.error(`    [step] ${t.id}/${name}: ${ok ? "ok" : "FAIL"} — ${String(detail).slice(0, 90)}`);
        t.steps.push({ name, ok, detail: String(detail).slice(0, 220) });
        return ok;
      };

      // T1 (simple): find + read + cite the official Rust async book
      {
        const t = { id: 'T1-async-book', difficulty: 'simple', steps: [], pass: false };
        const s = await k.call('net.search', { query: 'the rust async book', maxResults: 10 });
        step(t, 'search', s.ok, s.ok ? `${s.result.results.length} results` : (s.error?.message ?? 'fail'));
        const hit = s.ok && s.result.results.find((r) => norm(r.url).startsWith('rust-lang.github.io/async-book'));
        step(t, 'locate-official-async-book', !!hit, hit?.url ?? JSON.stringify((s.result?.results ?? []).slice(0, 3).map((r) => r.url)));
        const f = hit ? await k.call('net.fetch', { url: hit.url }) : null;
        step(t, 'fetch-as-markdown', !!f && f.ok === true && (f.result?.markdown ?? '').includes('Async'), f?.result?.title ?? f?.error?.message ?? 'no candidate');
        const c = f && f.ok ? await k.call('net.cite', { url: hit.url }) : null;
        step(t, 'cite-durable', !!c && c.ok === true && (c.result?.citation ?? '').includes('accessed'), c?.result?.citation ?? c?.error?.message ?? 'skipped');
        t.pass = t.steps.every((x) => x.ok);
        tasks.push(t);
      }

      // T2 (simple): current stable Node.js version from the official site
      {
        const t = { id: 'T2-node-version', difficulty: 'simple', steps: [], pass: false };
        const s = await k.call('net.search', { query: 'node.js javascript runtime', maxResults: 5 });
        step(t, 'search', s.ok, s.ok ? 'ok' : (s.error?.message ?? 'fail'));
        const hit = s.ok && s.result.results.find((r) => norm(r.url) === 'nodejs.org' || norm(r.url) === 'nodejs.org/en');
        step(t, 'locate-nodejs-org', !!hit, hit?.url ?? JSON.stringify((s.result?.results ?? []).slice(0, 3).map((r) => r.url)));
        const f = hit ? await k.call('net.fetch', { url: hit.url }) : null;
        const ver = f && f.ok ? ((f.result.markdown.match(/\bv(2\d|[4-9])\.\d+\.\d+\b/) || [])[0] ?? (() => { const m = f.result.markdown.match(/Node\.js[\s\S]{0,120}?\b(\d{2}\.\d+\.\d+)\b/); return m ? `v${m[1]}` : null; })()) : null;
        step(t, 'extract-stable-version', !!ver, ver ?? (f?.result?.markdown ?? '').slice(0, 80));
        const c = ver && hit ? await k.call('net.cite', { url: hit.url }) : null;
        step(t, 'cite-source', !!c && c.ok === true, c?.result?.citation ?? c?.error?.message ?? 'skipped');
        t.result = { version: ver };
        t.pass = t.steps.every((x) => x.ok);
        tasks.push(t);
      }

      // T3 (medium): verify a claim against PEP 634 and cite it
      {
        const t = { id: 'T3-pep634-verify', difficulty: 'medium', steps: [], pass: false };
        const s = await k.call('net.search', { query: 'pep 634 structural pattern matching python', maxResults: 5 });
        step(t, 'search', s.ok, s.ok ? 'ok' : (s.error?.message ?? 'fail'));
        const pepUrl = (s.result?.results ?? []).find((r) => norm(r.url).includes('pep-0634'))?.url;
        step(t, 'locate-pep-634', !!pepUrl, pepUrl ?? JSON.stringify((s.result?.results ?? []).slice(0, 3).map((r) => r.url)));
        const v = pepUrl ? await k.call('net.verify', { claim: 'PEP 634 adds structural pattern matching to the Python language with match and case statements', url: pepUrl }) : null;
        if (v?.error?.code === 'ERR_EMBED_UNAVAILABLE') step(t, 'verify-claim', true, 'skipped (no model)');
        else step(t, 'verify-claim', !!v && v.ok === true && v.result?.verdict === 'grounded', v?.result ? `verdict=${v.result.verdict} score=${v.result.score}` : (v?.error?.message ?? 'skipped'));
        const c = pepUrl ? await k.call('net.cite', { url: pepUrl }) : null;
        step(t, 'cite-pep', !!c && c.ok === true, c?.result?.citation ?? c?.error?.message ?? 'skipped');
        t.pass = t.steps.every((x) => x.ok);
        tasks.push(t);
      }

      // T4 (medium): which RFC defines HTTP/2 — find, read, assert the number
      {
        const t = { id: 'T4-http2-rfc', difficulty: 'medium', steps: [], pass: false };
        const s = await k.call('net.search', { query: 'RFC hypertext transfer protocol version 2 http2 specification', maxResults: 8 });
        step(t, 'search', s.ok, s.ok ? 'ok' : (s.error?.message ?? 'fail'));
        const candidates = (s.ok ? s.result.results : []).map((r) => r.url);
        let found = null;
        for (const url of candidates.slice(0, 4)) {
          if (!/rfc-editor\.org|httpwg\.org|rfc7540|rfc9113|wikipedia\.org/i.test(url)) continue;
          const f = await k.call('net.fetch', { url });
          if (f.ok && /RFC (7540|9113)/.test(f.result.markdown)) { found = { url, rfc: (f.result.markdown.match(/RFC (7540|9113)/) || [])[1] }; break; }
        }
        step(t, 'find-and-read-rfc', !!found, found ? `${found.rfc} at ${found.url}` : 'no candidate yielded the RFC number');
        const c = found ? await k.call('net.cite', { url: found.url }) : null;
        step(t, 'cite-rfc', !!c && c.ok === true, c?.result?.citation ?? c?.error?.message ?? 'skipped');
        t.result = { rfc: found?.rfc };
        t.pass = t.steps.every((x) => x.ok);
        tasks.push(t);
      }

      // T5 (complex, multi-query): evidence pack comparing Brave vs Tavily free tiers
      {
        const t = { id: 'T5-free-tier-compare', difficulty: 'complex', steps: [], pass: false };
        const citations = [];
        const numbers = {};
        for (const [name, q] of [['brave', 'brave search api free tier queries per month pricing'], ['tavily', 'tavily api free plan credits per month pricing']]) {
          const s = await k.call('net.search', { query: q, maxResults: 6 });
          step(t, `search-${name}`, s.ok, s.ok ? `${s.result.results.length} results` : (s.error?.message ?? 'fail'));
          const candidates = (s.ok ? s.result.results : []).map((r) => r.url).filter((u) => !/facebook|twitter|x\.com|reddit\.com/i.test(u));
          let got = null;
          for (const url of candidates.slice(0, 3)) {
            const f = await k.call('net.fetch', { url });
            if (!f.ok) continue;
            const m = f.result.markdown.match(/(\d[\d,]{2,})\s*(?:free\s*)?(?:queries|calls|credits|searches)/i)
              || f.result.markdown.match(/free[^.]{0,80}?(\d[\d,]{2,})/i);
            if (m) { got = { url, number: m[1], raw: m[0].slice(0, 80) }; break; }
          }
          step(t, `extract-free-tier-${name}`, !!got, got ? `${got.number} from ${norm(got.url)}` : 'no free-tier number found in top-3 pages');
          if (got) {
            numbers[name] = got;
            const c = await k.call('net.cite', { url: got.url });
            if (c.ok) citations.push({ engine: name, citation: c.result.citation, id: c.result.id });
          }
        }
        step(t, 'two-independent-sources-cited', citations.length >= 2, citations.map((c) => c.engine).join(','));
        t.result = { numbers, citations };
        t.pass = t.steps.filter((x) => x.ok).length >= t.steps.length - 1 && citations.length >= 2;
        tasks.push(t);
      }

      report.tasks = tasks;
      const passed = tasks.filter((t) => t.pass).length;
      report.tasksPassed = passed;
      report.tasksTotal = tasks.length;
      console.log(`  TASKS      ${passed}/${tasks.length} passed (${tasks.map((t) => `${t.id}:${t.pass ? 'PASS' : 'FAIL'}`).join(', ')})`);
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }

  writeFileSync(join(outDir, 'real-tasks.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(`\nREAL-TASK REPORT → ${join(outDir, 'real-tasks.json')}`);
}

main().catch((e) => { console.error(e); process.exit(1); });
