// Wave W-Net-1 offline tests: the FULL net.fetch/net.robots pipeline against a
// local HTTP server (no external network). Includes the extraction verifier
// arm: token-F1 of extracted markdown vs hand-gold markdown ≥ 0.90, plus the
// SSRF fail-closed gate and cache/ETag revalidation behavior.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { withKernel } from './driver.mjs';

// ---- fixtures ---------------------------------------------------------------

const PAGE_TITLE = 'Indexing the Agent Web — Research Notes';
const P1 = 'The first wave of agent tooling treated the web as a bag of bytes: fetch a URL, dump raw HTML into the context window, and let the model fight the markup. That approach burns tokens on navigation menus, cookie banners, and footer boilerplate that no reader ever needed.';
const P2A = 'This note proposes the opposite contract. A fetch tool should return clean markdown with';
const P2B = 'the main content isolated by scoring blocks against link density and boilerplate signals';
const P2C = 'and a confidence value the agent can act on before spending a round trip.';
const P3 = 'Evaluation uses a fixed corpus with hand-verified gold documents. A verifier that cannot fail is decoration, so the gate also reports the mutated-gold negative control.';
const LIST = ['search backends and their fusion strategy', 'extraction heuristics and their failure modes', 'politeness, robots, and the modern signal layer'];
const CODE = 'const docs = await net.fetch(url);\nif (docs.extractionConfidence < 0.5) docs = await render(url);';
const P4 = 'The result is a fetch tool that behaves like a careful human reader: skim the chrome once, keep the content, and say how sure it is.';

function articleHtml(port) {
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>${PAGE_TITLE} | Example Press</title>
  <meta property="og:title" content="${PAGE_TITLE}">
  <meta property="og:site_name" content="Example Press">
  <meta name="author" content="A. Researcher">
</head>
<body>
  <nav class="site-nav"><a href="/">Home</a> <a href="/archive">Archive</a> <a href="/about">About</a></nav>
  <header class="masthead"><h1>Example Press</h1></header>
  <article>
    <h1>${PAGE_TITLE}</h1>
    <p>${P1}</p>
    <p>${P2A} <strong>${P2B}</strong> ${P2C}</p>
    <p>See the <a href="/related/standards">standards note</a> and the <a href="https://example.org/external">external survey</a>.</p>
    <h2>Method</h2>
    <ul>
      <li>${LIST[0]}</li>
      <li>${LIST[1]}</li>
      <li>${LIST[2]}</li>
    </ul>
    <pre><code class="language-javascript">${CODE}</code></pre>
    <p>${P3}</p>
    <h2>Results</h2>
    <p>${P4}</p>
  </article>
  <aside class="promo-box">Subscribe to the newsletter for weekly updates and sponsored deals!</aside>
  <footer class="site-footer"><p>© 2026 Example Press · Privacy · Terms</p></footer>
</body>
</html>`;
}

// Hand-gold markdown: exactly what the extractor should produce for /article.
function articleGold(port) {
  return [
    `# ${PAGE_TITLE}`,
    P1,
    `${P2A} **${P2B}** ${P2C}`,
    `See the [standards note](http://127.0.0.1:${port}/related/standards) and the [external survey](https://example.org/external).`,
    '# Method',
    `- ${LIST[0]}\n- ${LIST[1]}\n- ${LIST[2]}`,
    '```javascript\n' + CODE + '\n```',
    P3,
    '# Results',
    P4,
  ].join('\n\n');
}

const SHELL_HTML = '<!DOCTYPE html><html><head><title>SPA Shell</title></head><body><div id="app"></div><script src="/bundle.js"></script></body></html>';
const ROBOTS_TXT = 'User-agent: *\nDisallow: /private/\nAllow: /private/ok\nSitemap: http://127.0.0.1:PORT/sitemap.xml\n\nUser-agent: badbot\nDisallow: /\n';
const LLMS_TXT = '# Example Press\n\n> Curated index for agents.\n\n## Docs\n- [Standards note](/related/standards): the modern signal layer\n- [Archive](/archive): all research notes\n';

// ---- helpers ----------------------------------------------------------------

function tokens(s) {
  return new Set(
    s
      .toLowerCase()
      .split(/[^a-z0-9]+/)
      .filter((w) => w.length > 0),
  );
}

/** Token-set F1 — the extraction verifier's score. */
function f1(actual, gold) {
  const a = tokens(actual);
  const g = tokens(gold);
  let inter = 0;
  for (const t of a) if (g.has(t)) inter += 1;
  if (a.size === 0 || g.size === 0) return 0;
  const precision = inter / a.size;
  const recall = inter / g.size;
  return (2 * precision * recall) / (precision + recall);
}

const GOLD_REPORT = { cases: [], allPass: false }; // surfaced by the verifier test below

// ---- server -----------------------------------------------------------------

function startServer() {
  const counters = { etagHits: 0, etag304: 0 };
  const mutableCited = { flipped: false };
  const server = createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    const path = url.pathname;
    if (path === '/article') {
      const port = req.socket.localPort;
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end(articleHtml(port));
    } else if (path === '/minimal') {
      res.writeHead(200, { 'content-type': 'text/html' });
      res.end('<html><body><p>A minimal page with one paragraph of text.</p></body></html>');
    } else if (path === '/shell') {
      res.writeHead(200, { 'content-type': 'text/html' });
      res.end(SHELL_HTML);
    } else if (path === '/redirect') {
      res.writeHead(302, { location: '/article' });
      res.end();
    } else if (path === '/md') {
      res.writeHead(200, { 'content-type': 'text/markdown; charset=utf-8', 'x-markdown-tokens': '42', 'content-signal': 'search=yes, ai-input=yes' });
      res.end('# Negotiated\n\nThis page served markdown because the agent asked for it.');
    } else if (path === '/citable') {
      // mutable page for the cite lifecycle: /mutate flips its body
      const body = ctx.mutableCited.flipped
        ? 'The quarterly deployment cadence changed to weekly releases in Q3 after the incident review.'
        : 'The quarterly deployment cadence stayed monthly through Q2 with no changes after the incident review.';
      res.writeHead(200, { 'content-type': 'text/html' });
      res.end(`<html><body><article><h1>Deployment Policy</h1><p>${body}</p></article></body></html>`);
    } else if (path === '/mutate') {
      ctx.mutableCited.flipped = !ctx.mutableCited.flipped;
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end('mutated');
    } else if (path === '/json') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ok: true, items: [1, 2, 3] }));
    } else if (path === '/etag') {
      counters.etagHits += 1;
      if (req.headers['if-none-match'] === '"v1"') {
        counters.etag304 += 1;
        res.writeHead(304, { etag: '"v1"' });
        res.end();
      } else {
        res.writeHead(200, { 'content-type': 'text/html', etag: '"v1"', 'last-modified': 'Tue, 01 Sep 2026 00:00:00 GMT' });
        res.end('<html><body><p>Etag revalidation page content lives here with enough words to extract.</p></body></html>');
      }
    } else if (path === '/robots.txt') {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(ROBOTS_TXT.replaceAll('PORT', String(req.socket.localPort)));
    } else if (path === '/llms.txt') {
      res.writeHead(200, { 'content-type': 'text/markdown' });
      res.end(LLMS_TXT);
    } else if (path === '/sitemap.xml') {
      res.writeHead(200, { 'content-type': 'application/xml' });
      res.end('<?xml version="1.0"?><urlset></urlset>');
    } else {
      res.writeHead(404, { 'content-type': 'text/plain' });
      res.end('not found');
    }
  });
  return new Promise((resolvePromise) => {
    server.listen(0, '127.0.0.1', () => resolvePromise({ server, port: server.address().port, counters, mutableCited }));
  });
}

let ctx;
test.before(async () => { ctx = await startServer(); });
test.after(() => { ctx.server.close(); });

const base = () => `http://127.0.0.1:${ctx.port}`;

// ---- tests ------------------------------------------------------------------

test('net.fetch extracts the article: noise gone, structure kept, F1 ≥ 0.90', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.fetch', { url: `${base()}/article`, allowPrivate: true });
    assert.equal(r.ok, true, JSON.stringify(r.error));
    const md = r.result.markdown;

    // main content present
    for (const fragment of [P1, P2B, P3, P4]) assert.ok(md.includes(fragment), `missing main text: ${fragment.slice(0, 40)}`);
    // noise absent
    assert.ok(!md.includes('Subscribe to the newsletter'), 'promo box leaked into extraction');
    assert.ok(!md.includes('Privacy · Terms'), 'footer leaked into extraction');
    assert.ok(!md.includes('Masthead'), 'header leaked');
    assert.ok(!md.includes('site-nav'), 'nav leaked');
    // structure
    assert.ok(md.includes('# ' + PAGE_TITLE), 'h1 missing');
    assert.ok(md.includes('## Method'), 'h2 missing');
    assert.ok(md.includes('```javascript'), 'code fence with language missing');
    assert.ok(md.includes(`[standards note](http://127.0.0.1:${ctx.port}/related/standards)`), 'relative link not absolutized');
    assert.ok(md.includes('[external survey](https://example.org/external)'), 'absolute link broken');
    // metadata
    assert.equal(r.result.title, PAGE_TITLE);
    assert.ok(r.result.extractionConfidence >= 0.5, `confidence too low: ${r.result.extractionConfidence}`);
    assert.ok(r.result.tokens > 50, 'token estimate implausibly low');
    assert.equal(r.result.source, 'extracted');

    // verifier arm: token-F1 vs hand-gold
    const score = f1(md, articleGold(ctx.port));
    GOLD_REPORT.cases.push({ page: 'article', f1: score, gate: 0.9, pass: score >= 0.9 });
    assert.ok(score >= 0.9, `extraction F1 ${score.toFixed(3)} < 0.90\n--- extracted ---\n${md.slice(0, 2000)}\n--- gold ---\n${articleGold(ctx.port).slice(0, 2000)}`);
  });
});

test('net.fetch SSRF guard fails closed (fetch + http blockPrivate) and allowPrivate opens it', async () => {
  const root = mkdtempSync(join(tmpdir(), 'nct-net-'));
  try {
    await withKernel(root, async (k) => {
      const blocked = await k.call('net.fetch', { url: `${base()}/minimal` });
      assert.equal(blocked.ok, false);
      assert.equal(blocked.error.code, 'ERR_SSRF_BLOCKED', JSON.stringify(blocked.error));

      const httpBlocked = await k.call('net.http', { url: `${base()}/minimal`, blockPrivate: true });
      assert.equal(httpBlocked.ok, false);
      assert.equal(httpBlocked.error.code, 'ERR_SSRF_BLOCKED');

      // metadata 169.254.169.254 (the classic SSRF target) must refuse to resolve
      const meta = await k.call('net.fetch', { url: 'http://169.254.169.254/latest/meta-data/' });
      assert.equal(meta.ok, false);
      assert.equal(meta.error.code, 'ERR_SSRF_BLOCKED');

      // default net.http keeps raw-curl semantics (reaches loopback)
      const raw = await k.call('net.http', { url: `${base()}/minimal` });
      assert.equal(raw.ok, true);
      assert.ok(raw.result.body.includes('minimal page'));

      // allowPrivate opens the guarded path
      const allowed = await k.call('net.fetch', { url: `${base()}/minimal`, allowPrivate: true });
      assert.equal(allowed.ok, true);
      assert.ok(allowed.result.markdown.includes('minimal page'));
    });
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('net.fetch follows redirects with a chain report and re-guards each hop', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.fetch', { url: `${base()}/redirect`, allowPrivate: true });
    assert.equal(r.ok, true, JSON.stringify(r.error));
    assert.equal(r.result.finalUrl, `${base()}/article`);
    assert.equal(r.result.redirects.length, 1);
    assert.ok(r.result.markdown.includes(P1));
  });
});

test('net.fetch cache: miss → hit → refresh revalidates via ETag 304', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const url = `${base()}/etag`;
    const first = await k.call('net.fetch', { url, allowPrivate: true });
    assert.equal(first.result.cache, 'miss');
    assert.ok(first.result.contentHash, 'content hash missing');

    const second = await k.call('net.fetch', { url, allowPrivate: true });
    assert.equal(second.result.cache, 'hit');
    assert.equal(second.result.contentHash, first.result.contentHash, 'hash must be stable across cache hits');
    // structural (not timing) hit check: no network round-trip → status is null
    assert.equal(second.result.status, null, 'cache hit must not re-report HTTP status');

    const third = await k.call('net.fetch', { url, allowPrivate: true, refresh: true });
    assert.equal(third.result.cache, 'revalidated', JSON.stringify(third.result));
    assert.equal(ctx.counters.etag304, 1, 'server must have seen If-None-Match and answered 304');
  });
});

test('net.fetch honors markdown content negotiation and JSON sources', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const md = await k.call('net.fetch', { url: `${base()}/md`, allowPrivate: true });
    assert.equal(md.result.source, 'markdown-negotiated');
    assert.ok(md.result.markdown.includes('# Negotiated'));
    assert.equal(md.result.contentSignals, 'search=yes, ai-input=yes');

    const js = await k.call('net.fetch', { url: `${base()}/json`, allowPrivate: true });
    assert.equal(js.result.source, 'json');
    assert.ok(js.result.markdown.startsWith('```json'));
  });
});

test('net.fetch reports JS-shell pages honestly (low confidence, empty content)', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.fetch', { url: `${base()}/shell`, allowPrivate: true });
    assert.equal(r.ok, true);
    assert.ok(r.result.markdown.trim().length < 200, 'shell page should yield near-empty content');
    assert.ok(r.result.extractionConfidence <= 0.5, 'shell page must not claim high confidence');
  });
});

test('net.robots: isAllowed + sitemaps + llms.txt discovery', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.robots', { url: `${base()}/private/secret`, allowPrivate: true });
    assert.equal(r.ok, true, JSON.stringify(r.error));
    assert.equal(r.result.allowed, false);
    assert.deepEqual(r.result.sitemaps, [`${base()}/sitemap.xml`]);
    assert.equal(r.result.llmsTxt.found, true);
    assert.ok(r.result.llmsTxt.content.includes('Curated index for agents'));

    const ok = await k.call('net.robots', { url: `${base()}/article`, allowPrivate: true });
    assert.equal(ok.result.allowed, true);
  });
});

test('net.search validates input and engines before touching the network', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const empty = await k.call('net.search', { query: '   ' });
    assert.equal(empty.ok, false);
    assert.equal(empty.error.code, 'ERR_BAD_INPUT');

    const badEngine = await k.call('net.search', { query: 'rust', engines: 'ddg,nope' });
    assert.equal(badEngine.ok, false);
    assert.equal(badEngine.error.code, 'ERR_BAD_INPUT');
    assert.ok(badEngine.error.hint.known.includes('ddg'));
  });
});

test('net.fetch provenance lands in the journal', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    await k.call('net.fetch', { url: `${base()}/minimal`, allowPrivate: true });
    const j = await k.call('sys.journal', {});
    const events = j.result.events ?? j.result;
    const evts = Array.isArray(events) ? events : events.last ?? [];
    const fetchEvt = evts.find((e) => e.kind === 'net.fetch');
    assert.ok(fetchEvt, 'net.fetch journal event missing');
    assert.equal(fetchEvt.url, `${base()}/minimal`);
    assert.ok(fetchEvt.hash);
  });
});

// verifier bookkeeping export (consumed by tools/net-verifier.mjs via a fresh run)
test('extraction verifier summary is self-checking (mutated gold must FAIL)', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.fetch', { url: `${base()}/article`, allowPrivate: true });
    // negative control: gold with half its sentences REMOVED — overlap must
    // collapse below the gate (a verifier that cannot fail is decoration)
    const mutated = articleGold(ctx.port).split('\n\n').filter((_, i) => i % 2 === 0).join('\n\n');
    const score = f1(r.result.markdown, mutated);
    GOLD_REPORT.cases.push({ page: 'article-mutated-control', f1: score, gate: 0.9, pass: score >= 0.9 });
    assert.ok(score < 0.9, `mutated-gold negative control must fail (got F1 ${score.toFixed(3)})`);
  });
});

// ---- W-Net-2a: citation ledger + grounding verification -----------------------

test('net.cite lifecycle: create → list → idempotent re-cite → health unchanged → content-changed after mutation', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const url = `${base()}/citable`;
    const c1 = await k.call('net.cite', { url, allowPrivate: true });
    assert.equal(c1.ok, true, JSON.stringify(c1.error));
    assert.ok(c1.result.id, 'citation id missing');
    assert.ok(c1.result.citation.includes('accessed '), 'citation line missing access date');
    assert.ok(/[a-f0-9]{8}/.test(c1.result.citation), 'citation line missing hash prefix');
    assert.equal(c1.result.duplicate, false);

    const list = await k.call('net.cite', {});
    assert.equal(list.result.total, 1);
    assert.equal(list.result.sources[0].id, c1.result.id);

    // same content again → duplicate:true, same id (idempotent)
    const c2 = await k.call('net.cite', { url, allowPrivate: true });
    assert.equal(c2.result.id, c1.result.id);
    assert.equal(c2.result.duplicate, true);

    // health check: source unchanged
    const h1 = await k.call('net.cite', { id: c1.result.id, allowPrivate: true });
    assert.equal(h1.result.health, 'unchanged', JSON.stringify(h1.result));

    // silent site edit → NEW id on re-cite, health flips to changed on old id
    const flip = await k.call('net.http', { url: `${base()}/mutate` });
    assert.equal(flip.ok, true);
    const c3 = await k.call('net.cite', { url, allowPrivate: true });
    assert.notEqual(c3.result.id, c1.result.id, 'content change must produce a new citation id');
    const h2 = await k.call('net.cite', { id: c1.result.id, allowPrivate: true });
    assert.equal(h2.result.health, 'changed', 'old citation must detect the content change');

    const list2 = await k.call('net.cite', {});
    assert.equal(list2.result.total, 2);
  });
});

test('net.cite unknown id → ERR_NOT_FOUND with known ids hint', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const r = await k.call('net.cite', { id: 'nope1234' });
    assert.equal(r.ok, false);
    assert.equal(r.error.code, 'ERR_NOT_FOUND');
  });
});

test('net.verify grounds a true claim and rejects a contradicting one (model available)', async (t) => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    // the static article fixture (never mutated by other tests)
    const url = `${base()}/article`;
    const grounded = await k.call('net.verify', {
      claim: 'a fetch tool should return clean markdown with isolated main content and a confidence value',
      url, allowPrivate: true,
    });
    if (grounded.error?.code === 'ERR_EMBED_UNAVAILABLE') { t.skip('no local model'); return; }
    assert.equal(grounded.ok, true, JSON.stringify(grounded.error));
    assert.equal(grounded.result.verdict, 'grounded', JSON.stringify(grounded.result));
    assert.ok(grounded.result.score >= 0.5, `grounded score below gate: ${grounded.result.score}`);
    assert.ok(grounded.result.bestSpan.length > 0, 'grounding span missing');
    assert.ok(grounded.result.contentHash, 'content hash missing on verify');

    // negative control: an unrelated claim against the same page
    const contradicted = await k.call('net.verify', {
      claim: 'best recipe for sourdough bread with starter ratios',
      url, allowPrivate: true,
    });
    assert.equal(contradicted.ok, true);
    assert.ok(['not-grounded', 'partial'].includes(contradicted.result.verdict),
      `unrelated claim must not be grounded (got ${contradicted.result.verdict})`);

    // verify by citation id path: grounds against the LEDGER's stored copy
    const cite = await k.call('net.cite', { url, allowPrivate: true });
    const byId = await k.call('net.verify', {
      claim: 'a fetch tool should return clean markdown with isolated main content and a confidence value',
      id: cite.result.id,
    });
    assert.equal(byId.ok, true, JSON.stringify(byId.error));
    assert.equal(byId.result.verdict, 'grounded');
  });
});

test('net.verify validates input (empty claim, missing source, unknown id)', async () => {
  await withKernel(mkdtempSync(join(tmpdir(), 'nct-net-')), async (k) => {
    const empty = await k.call('net.verify', { claim: '   ', url: `${base()}/citable`, allowPrivate: true });
    assert.equal(empty.ok, false);
    assert.equal(empty.error.code, 'ERR_BAD_INPUT');

    const noSrc = await k.call('net.verify', { claim: 'something true' });
    assert.equal(noSrc.ok, false);
    assert.equal(noSrc.error.code, 'ERR_BAD_INPUT');

    const unknown = await k.call('net.verify', { claim: 'x', id: 'ghost000' });
    assert.equal(unknown.ok, false);
    assert.equal(unknown.error.code, 'ERR_NOT_FOUND');
  });
});
