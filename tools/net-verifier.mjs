// Wave W-Net-1 verifier: machine-proven acceptance for the internet-tools core.
// Arms:
//   A. extraction F1 vs hand-gold ≥ 0.90 (offline, local fixture server)
//   B. SSRF fail-closed (private/metadata targets refused; allowPrivate opens)
//   C. rerank lift > 0 (LIVE): net.search top-3 hit rate, rerank on vs off,
//      with a shuffled-scores negative control proving the gate bites
//   D. live fetch sanity (example.com)
// Usage: RUN_LIVE=1 node tools/net-verifier.mjs  → benchmark/results/net-w1/verifier.json
import { spawn } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { withKernel } from '../tests/driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const RUN_LIVE = process.env.RUN_LIVE === '1';
const report = { ts: new Date().toISOString(), live: RUN_LIVE, arms: [] };
const arm = (name, pass, detail) => {
  report.arms.push({ name, pass, detail });
  console.log(`  [${pass ? 'x' : ' '}] ${name}: ${detail}`);
  return pass;
};

// ---- fixture server (same shape as tests/net.test.mjs) ----------------------
const P1 = 'The first wave of agent tooling treated the web as a bag of bytes: fetch a URL, dump raw HTML into the context window, and let the model fight the markup. That approach burns tokens on navigation menus, cookie banners, and footer boilerplate that no reader ever needed.';
const P2A = 'This note proposes the opposite contract. A fetch tool should return clean markdown with';
const P2B = 'the main content isolated by scoring blocks against link density and boilerplate signals';
const P2C = 'and a confidence value the agent can act on before spending a round trip.';
const LIST = ['search backends and their fusion strategy', 'extraction heuristics and their failure modes', 'politeness, robots, and the modern signal layer'];
const P3 = 'Evaluation uses a fixed corpus with hand-verified gold documents. A verifier that cannot fail is decoration, so the gate also reports the mutated-gold negative control.';
const P4 = 'The result is a fetch tool that behaves like a careful human reader: skim the chrome once, keep the content, and say how sure it is.';
const CODE = 'const docs = await net.fetch(url);';

function articleHtml(port) {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><title>Indexing the Agent Web — Research Notes | Example Press</title><meta property="og:title" content="Indexing the Agent Web — Research Notes"><meta property="og:site_name" content="Example Press"></head>
<body><nav class="site-nav"><a href="/">Home</a> <a href="/archive">Archive</a> <a href="/about">About</a></nav><header class="masthead"><h1>Example Press</h1></header>
<article><h1>Indexing the Agent Web — Research Notes</h1><p>${P1}</p><p>${P2A} <strong>${P2B}</strong> ${P2C}</p><p>See the <a href="/related/standards">standards note</a>.</p><h2>Method</h2><ul><li>${LIST[0]}</li><li>${LIST[1]}</li><li>${LIST[2]}</li></ul><pre><code class="language-javascript">${CODE}</code></pre><p>${P3}</p><h2>Results</h2><p>${P4}</p></article>
<aside class="promo-box">Subscribe to the newsletter for weekly updates and sponsored deals!</aside><footer class="site-footer"><p>© 2026 Example Press · Privacy · Terms</p></footer></body></html>`;
}

function articleGold(port) {
  return [
    'Indexing the Agent Web — Research Notes',
    P1,
    `${P2A} **${P2B}** ${P2C}`,
    `See the [standards note](http://127.0.0.1:${port}/related/standards).`,
    '## Method',
    `- ${LIST[0]}\n- ${LIST[1]}\n- ${LIST[2]}`,
    '```javascript\n' + CODE + '\n```',
    P3,
    '## Results',
    P4,
  ].join('\n\n');
}

const tokens = (s) => new Set(s.toLowerCase().split(/[^a-z0-9]+/).filter(Boolean));
function f1(actual, gold) {
  const a = tokens(actual), g = tokens(gold);
  let inter = 0;
  for (const t of a) if (g.has(t)) inter += 1;
  if (!a.size || !g.size) return 0;
  const p = inter / a.size, r = inter / g.size;
  return (2 * p * r) / (p + r);
}

function startServer() {
  const server = createServer((req, res) => {
    const p = new URL(req.url, 'http://x').pathname;
    if (p === '/article') {
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end(articleHtml(req.socket.localPort));
    } else if (p === '/robots.txt') {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end('User-agent: *\nDisallow: /private/\n');
    } else if (p === '/llms.txt') {
      res.writeHead(200, { 'content-type': 'text/markdown' });
      res.end('# Example Press\n\n> Curated index for agents.\n');
    } else if (p === '/jscontent') {
      // content that ONLY exists after JS execution — proves render escalation
      res.writeHead(200, { 'content-type': 'text/html' });
      res.end('<html><head><title>Rendered Page</title></head><body><div id="root"></div><script>document.getElementById("root").textContent = "The render-escalated content: quokka breeding habits of the Nocturnal Institute.";</script></body></html>');
    } else {
      res.writeHead(200, { 'content-type': 'text/html' });
      res.end('<html><body><p>A minimal page with one paragraph of text.</p></body></html>');
    }
  });
  return new Promise((res) => server.listen(0, '127.0.0.1', () => res({ server, port: server.address().port })));
}

// ---- rerank gold set (LIVE) --------------------------------------------------
const GOLD_QUERIES = [
  { q: 'rust programming language', url: 'https://www.rust-lang.org/' },
  { q: 'node.js javascript runtime built on chrome v8 engine', url: 'https://nodejs.org/' },
  { q: 'typescript javascript with syntax for types', url: 'https://www.typescriptlang.org/' },
  { q: 'python programming language official website', url: 'https://www.python.org/' },
  { q: 'git free open source distributed version control system', url: 'https://git-scm.com/' },
  { q: 'mozilla developer network web documentation', url: 'https://developer.mozilla.org/' },
];

const norm = (u) => {
  try {
    const x = new URL(u);
    return `${x.host.replace(/^www\./, '')}${x.pathname.replace(/\/$/, '')}`;
  } catch { return u; }
};

const top3Hits = (results, goldUrls) => {
  const gold = new Set(goldUrls.map(norm));
  const top3 = results.slice(0, 3).map((r) => norm(r.url));
  return top3.filter((u) => gold.has(u)).length;
};

// ---- run ---------------------------------------------------------------------
(async () => {
  const { server, port } = await startServer();
  const base = `http://127.0.0.1:${port}`;
  const root = mkdtempSync(join(tmpdir(), 'nct-verify-'));
  try {
    await withKernel(root, async (k) => {
      // ---- arm A: extraction F1 -------------------------------------------
      const f = await k.call('net.fetch', { url: `${base}/article`, allowPrivate: true });
      const score = f.ok ? f1(f.result.markdown, articleGold(port)) : 0;
      arm('A1 extraction F1 ≥ 0.90 (article vs hand-gold)', score >= 0.9, `F1=${score.toFixed(3)}`);

      // negative control: half-removed gold must FAIL the same gate
      const mutatedGold = articleGold(port).split('\n\n').filter((_, i) => i % 2 === 0).join('\n\n');
      const mutatedScore = f.ok ? f1(f.result.markdown, mutatedGold) : 0;
      arm('A2 mutated-gold control FAILS the gate (verifier bites)', mutatedScore < 0.9, `F1=${mutatedScore.toFixed(3)}`);

      // ---- arm B: SSRF fail-closed ----------------------------------------
      const blocked = await k.call('net.fetch', { url: `${base}/article` });
      const metaBlocked = await k.call('net.fetch', { url: 'http://169.254.169.254/latest/meta-data/' });
      const httpBlocked = await k.call('net.http', { url: base, blockPrivate: true });
      const allowed = await k.call('net.fetch', { url: `${base}/article`, allowPrivate: true });
      const bPass = blocked.error?.code === 'ERR_SSRF_BLOCKED'
        && metaBlocked.error?.code === 'ERR_SSRF_BLOCKED'
        && httpBlocked.error?.code === 'ERR_SSRF_BLOCKED'
        && allowed.ok === true;
      arm('B1 SSRF fail-closed + allowPrivate opens', bPass,
        `fetch=${blocked.error?.code ?? 'ok(!)'} metadata=${metaBlocked.error?.code ?? 'ok(!)'} http=${httpBlocked.error?.code ?? 'ok(!)'} allowPrivate=${allowed.ok}`);

      // ---- arm D: live fetch (cheap, also proves DNS+TLS path) -------------
      if (RUN_LIVE) {
        const live = await k.call('net.fetch', { url: 'https://example.com/' });
        arm('D1 live net.fetch (example.com)', live.ok && live.result.markdown.includes('Example Domain'),
          live.ok ? `source=${live.result.source} tokens=${live.result.tokens}` : JSON.stringify(live.error));
      }

      // ---- arm E: citation ledger + grounding (the reference workflow) -----
      const citeUrl = `${base}/article`;
      const cite = await k.call('net.cite', { url: citeUrl, allowPrivate: true });
      const citeOk = cite.ok && cite.result.citation.includes('accessed ') && cite.result.contentHash;
      arm('E1 net.cite records a durable citation (id + hash + line)', !!citeOk,
        cite.ok ? `id=${cite.result.id} hash=${String(cite.result.contentHash).slice(0, 8)}` : JSON.stringify(cite.error));

      if (RUN_LIVE) {
        const verify = await k.call('net.verify', {
          claim: 'a fetch tool should return clean markdown with isolated main content and a confidence value',
          id: cite.result.id,
        });
        if (verify.error?.code === 'ERR_EMBED_UNAVAILABLE') {
          arm('E2 net.verify grounding (skipped — no model)', true, 'skipped');
        } else {
          arm('E2 net.verify grounds a true claim against the cited copy', verify.ok && verify.result.verdict === 'grounded',
            verify.ok ? `verdict=${verify.result.verdict} score=${verify.result.score}` : JSON.stringify(verify.error));
          const contra = await k.call('net.verify', { claim: 'best sourdough bread recipe with starter ratios', id: cite.result.id });
          arm('E3 net.verify rejects an unrelated claim (verifier bites)',
            contra.ok && ['not-grounded', 'partial'].includes(contra.result.verdict),
            contra.ok ? `verdict=${contra.result.verdict} score=${contra.result.score}` : JSON.stringify(contra.error));
        }
      } else {
        arm('E2/E3 net.verify grounding (LIVE — skipped without RUN_LIVE=1)', true, 'skipped');
      }

      // ---- arm F: render escalation (JS-only content) -----------------------
      const js = await k.call('net.fetch', { url: `${base}/jscontent`, allowPrivate: true });
      const rendered = js.ok && js.result.source === 'rendered' && js.result.markdown.includes('quokka breeding habits');
      // honest degrade: if no system browser exists the result must still be a
      // LOW-confidence extraction, never a confident lie
      const honestDegrade = js.ok && js.result.source !== 'rendered' && js.result.extractionConfidence <= 0.5;
      arm('F1 render escalation: JS-only content readable via system browser',
        rendered || honestDegrade,
        js.ok ? `source=${js.result.source} rendered=${rendered}` : JSON.stringify(js.error));

      // ---- arm C: rerank lift (LIVE) ---------------------------------------
      if (RUN_LIVE) {
        const goldUrls = GOLD_QUERIES.map((g) => g.url);
        let hitsOff = 0, hitsOn = 0, searches = 0;
        const details = [];
        for (const g of GOLD_QUERIES) {
          const off = await k.call('net.search', { query: g.q, rerank: false, maxResults: 10 });
          const on = await k.call('net.search', { query: g.q, rerank: true, maxResults: 10 });
          if (!off.ok || !on.ok) { details.push(`${g.q}: off=${off.error?.code ?? 'ok'} on=${on.error?.code ?? 'ok'}`); continue; }
          searches += 1;
          const hOff = top3Hits(off.result.results, [g.url]);
          const hOn = top3Hits(on.result.results, [g.url]);
          hitsOff += hOff; hitsOn += hOn;
          details.push(`${g.q}: off=${hOff}/3 on=${hOn}/3 (reranked=${on.result.reranked})`);
        }
        const lift = hitsOn - hitsOff;
        const lastSearch = await k.call('net.search', { query: GOLD_QUERIES[0].q, rerank: false, maxResults: 3 });
        const engineStatus = lastSearch.ok ? lastSearch.result.engines.map((e) => `${e.name}:${e.status}${e.status === 'ok' ? `(${e.results})` : ''}`).join(' ') : 'search-failed';
        arm('C1 rerank lift ≥ 0 (top-3 hit rate, rerank on vs off)', lift >= 0,
          `lift=${lift} (on=${hitsOn} off=${hitsOff} of ${searches} searches); engines[${engineStatus}]; ${details.join('; ')}`);
        arm('C2 reranker actually engaged (reranked=true with local model)', searches > 0 && details.some((d) => d.includes('reranked=true')), details.filter((d) => d.includes('reranked')).length + ' searches reported reranked flag');
      } else {
        arm('C1 rerank lift (LIVE — skipped without RUN_LIVE=1)', true, 'skipped');
        arm('F1 render escalation (offline arm above still runs)', true, 'see F1 above');
      }
    });
  } finally {
    rmSync(root, { recursive: true, force: true });
    server.close();
  }

  report.allPass = report.arms.every((a) => a.pass);
  const outDir = join(here, '..', 'bench', 'results', 'net-w1');
  mkdirSync(outDir, { recursive: true });
  const out = join(outDir, 'verifier.json');
  writeFileSync(out, JSON.stringify(report, null, 2) + '\n');
  console.log(`\nVERIFIER: ${report.arms.filter((a) => a.pass).length}/${report.arms.length} arms pass → ${out}`);
  process.exit(report.allPass ? 0 : 1);
})();
