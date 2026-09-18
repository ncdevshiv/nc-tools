// Seedless "cheapest X in region" research — NO pre-known brand list.
// Phase 1 DISCOVER: run generic + review-site + comparison queries; harvest
// brand candidates from result URLs/Titles (domain extraction, not a seed
// list; only generic noise domains are excluded by pattern).
// Phase 2 VERIFY: for each candidate, find its pricing page (brand query →
// URL containing pricing/vps/plans) and fetch it; extract real prices with
// context. Output: every candidate with evidence — unbiased by construction.
//
// Usage: node bench/discover-research.mjs "<topic>" [maxBrands]
import { withKernel } from '../tools/kernel-client.mjs';
import { mkdtempSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const TOPIC = process.argv[2] ?? 'cheap VPS hosting India INR';
const MAX_BRANDS = Number(process.argv[3] ?? 14);

// Noise domains that are never themselves providers (aggregators, socials).
const NOISE = /wikipedia\.org|reddit\.com|youtube\.com|facebook\.com|instagram\.com|x\.com|twitter\.com|linkedin\.com|quora\.com|amazon\.(com|in)|flipkart\.com|medium\.com|github\.com|trustpilot\.com|g2\.com|capterra\.com|producthunt\.com|yelp\.com|justdial\.com|sulekha\.com|blogspot\.com|wordpress\.com|hostinger\.(com|in)$/i;

const hostOf = (u) => { try { return new URL(u).host.replace(/^www\./, ''); } catch { return null; } };

// Candidate name from host: "e2enetworks.com" → "E2E Networks" style guess
const nameFromHost = (h) => {
  const base = h.split('.')[0];
  return base.replace(/[-_]/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
};

const DISCOVERY_QUERIES = [
  `${TOPIC}`,
  `best ${TOPIC} providers`,
  `${TOPIC} comparison`,
  `${TOPIC} review`,
  // Bing RSS serves word-fragment garbage for long queries (live-observed:
  // "best cheap vps hosting india" matched the single word "best" →
  // dictionary entries). Short anchored queries are the workaround.
  `best VPS India`,
  `cheap VPS plans`,
  `VPS hosting review`,
];

const PRICE = /(?:₹|INR|Rs\.?|\$|€|USD|EUR)\s?[\d,]{2,}(?:\.\d+)?(?:\s*(?:\/|per\s*)\s*(?:mo|month|mon))?/gi;

async function main() {
  const root = mkdtempSync(join(tmpdir(), 'discover-'));
  const report = { topic: TOPIC, ts: new Date().toISOString(), phase1: {}, candidates: [], verified: [] };
  try {
    await withKernel(root, async (k) => {
      // ---------- PHASE 1: discover candidates (no seed list) ----------
      const candidates = new Map(); // host -> { name, evidence: [], mentions }
      for (const q of DISCOVERY_QUERIES) {
        const r = await k.call('net.search', { query: q, maxResults: 10 });
        const rows = r.ok ? r.result.results : [];
        console.error(`[discover] "${q}" → ${rows.length} results${r.ok ? ` (dropped=${(r.result.sourcesDropped || []).length})` : ` ERR=${r.error?.code}`}`);
        for (const res of rows) {
          const host = hostOf(res.url);
          if (!host || NOISE.test(host)) continue;
          const rec = candidates.get(host) ?? { name: nameFromHost(host), host, evidence: [], mentions: 0 };
          rec.mentions += 1;
          rec.evidence.push({ via: q, url: res.url, title: (res.title ?? '').slice(0, 90), engines: res.engines ?? [res.engine] });
          candidates.set(host, rec);
        }
      }
      // rank by mentions (organic frequency across independent queries)
      const ranked = [...candidates.values()].sort((a, b) => b.mentions - a.mentions).slice(0, MAX_BRANDS);
      report.phase1 = { queries: DISCOVERY_QUERIES.length, candidates: ranked.length };
      report.candidates = ranked.map((c) => ({ host: c.host, name: c.name, mentions: c.mentions, sample: c.evidence[0].url }));
      console.error(`[discover] ${candidates.size} unique hosts → top ${ranked.length}: ${ranked.map((c) => `${c.host}(${c.mentions})`).join(', ')}`);

      // ---------- PHASE 2: verify each candidate's pricing ----------
      for (const c of ranked) {
        const entry = { host: c.host, name: c.name, mentions: c.mentions, pricingUrl: null, prices: [], note: null };
        // find the pricing page: brand query restricted to the host
        const fq = `${c.name} ${TOPIC.split(' ').slice(0, 3).join(' ')} pricing`;
        const sr = await k.call('net.search', { query: fq, maxResults: 8 });
        const urls = [];
        if (sr.ok) for (const res of sr.result.results) if (hostOf(res.url) === c.host) urls.push(res.url);
        urls.push(`https://${c.host}`); // fallback: site root
        for (const url of [...new Set(urls)].slice(0, 4)) {
          const f = await k.call('net.fetch', { url, timeoutMs: 40000 });
          if (!f.ok) { entry.note = `fetch ${f.error?.code}`; continue; }
          const md = (f.result.markdown ?? '').replace(/\s+/g, ' ');
          const priceHits = [];
          const seen = new Set();
          for (const m of md.matchAll(PRICE)) {
            const ctx = md.slice(Math.max(0, m.index - 60), Math.min(md.length, m.index + 40)).trim();
            const key = ctx.slice(0, 45);
            if (seen.has(key)) continue;
            seen.add(key);
            priceHits.push(ctx);
            if (priceHits.length >= 6) break;
          }
          if (priceHits.length >= 2) {
            entry.pricingUrl = f.result.finalUrl;
            entry.prices = priceHits;
            entry.source = f.result.source;
            entry.confidence = f.result.extractionConfidence;
            break;
          }
        }
        report.verified.push(entry);
        console.error(`[verify] ${c.host}: ${entry.pricingUrl ? `${entry.prices.length} price lines @ ${entry.pricingUrl.slice(0, 60)}` : entry.note ?? 'no prices found'}`);
      }
    });
  } finally { rmSync(root, { recursive: true, force: true }); }

  const out = join(here, 'results', 'discover');
  mkdirSync(out, { recursive: true });
  const file = join(out, `${TOPIC.replace(/[^\w]+/g, '-').slice(0, 40).toLowerCase()}.json`);
  writeFileSync(file, JSON.stringify(report, null, 2) + '\n');
  console.log('\n=== VERIFIED (cheapest-first view is up to the reader; evidence attached) ===');
  for (const v of report.verified) {
    console.log(`\n${v.name} (${v.host}, mentions=${v.mentions})`);
    if (v.pricingUrl) { console.log(`  ${v.pricingUrl}`); for (const p of v.prices) console.log(`  · ${p}`); }
    else console.log(`  NO PRICES (${v.note ?? 'n/a'})`);
  }
  console.log(`\n→ ${file}`);
}
main().catch((e) => { console.error(e); process.exit(1); });
