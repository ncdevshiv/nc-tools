# Internet Tools: Research + Design (Wave W-Net)

Status: **W-Net-1 SHIPPED 2026-09-01** — `net.fetch` / `net.robots` / `net.search` live (60-tool surface), verifier 6/6 arms (`benchmark/results/net-w1/verifier.json`). W-Net-2/3 below remain design.

## W-Net-1 shipped results

- Surface: `net.fetch` (SSRF-guarded by default, ETag cache, `Accept: text/markdown` negotiation, trafilatura-style extraction with two-extractor consensus confidence, token estimate, redirect chain, journal provenance event), `net.robots` (RFC 9309 longest-match isAllowed, sitemaps, llms.txt probe), `net.search` (keyless engine fan-out → RRF fusion → URL-normalized dedupe → local MiniLM rerank with host signal; per-engine errors never fail the call; per-engine politeness gate), plus `net.http` `blockPrivate` option.
- Gates: extraction token-F1 vs hand-gold **1.000** (gate 0.90; mutated-gold control fails at 0.527 — the verifier bites), SSRF fail-closed on loopback/metadata/`net.http blockPrivate`, cache miss→hit→304-revalidate, conformance 27/27, node suite 100/100, cargo units 12/12 net (+workspace green), crossaudit 20/20.
- **Live engine status (2026-09-01, this environment):** HN Algolia (JSON) and Wikipedia (JSON) healthy; DDG Lite serves an anomaly-detection challenge (keyless scrape blocked); Mojeek returned results then 403/JS-shell after several requests. The risk register's "keyless engine HTML changes" prediction held — adapters are fixture-validated data, degrade to per-engine errors, and W-Net-2's keyed adapters (Brave/Tavily/Exa) are the recall fix. Rerank lift on navigational gold queries measured lift=0 (gold URLs absent from candidate sets — a recall problem upstream of ranking, recorded honestly).

---

## 1. Baseline: what we have today

| Tool | What it is | Gaps |
|---|---|---|
| `net.http` | Raw fetch wrapper: method/headers/body, 2 MB body cap, timeout, structured status+headers+body | No extraction, no SSRF guard, no cache, no robots, no redirects control, returns raw HTML the agent must parse |
| `net.probePort` | TCP connect check with latency | — |

The gap is not HTTP — it is everything above HTTP. An agent asking "what does this site say about X?" burns its own context parsing markup, gets JS-shell pages as empty strings, has no search entry point, and leaves no audit trail.

## 2. Landscape survey (primary-source verified)

### 2.1 Search backends

| Project | What it does | How | License | Take |
|---|---|---|---|---|
| **ddgs** (deedy5) | Metasearch library + REST + MCP + CLI | Fans a query across **10 backends** (bing, brave, ddg, google, grokipedia, mojeek, startpage, yandex, yahoo, wikipedia) for text; also images/videos/news/books and an `extract()` (URL→markdown). Backend=`auto` fan-out, proxy support | MIT | Proof that **keyless multi-backend search works**; but **zero ranking** — it just concatenates backends |
| **SearXNG** | Self-hosted metasearch | Aggregates up to **272 services** via pluggable engine classes (XPath scrape, MediaWiki, JSON API); typed results; limiter + `botdetection`; Redis/Valkey cache; ~70 public instances; JSON API | AGPL-3.0 | Best engine-abstraction blueprint; heavy Python service, **no neural ranking** |
| **Perplexica** (→Vane) | Open Perplexity clone | SearXNG search → LLM (Ollama/OpenAI/…) → cited answers; speed/balanced/quality modes | MIT | The pipeline shape for `net.research`, but it hard-depends on both a SearXNG instance and an LLM |
| **Tavily** (closed) | Agent-search API | search / extract / crawl / map / research-task endpoints; credit model; docs are llms.txt-first with an agents.md skill | proprietary | The endpoint **shape** is right; it is cloud + key + credits |
| **Exa** (closed) | Neural search for AIs | Latency-tiered search: `instant` ~250 ms → `deep-reasoning` 12–40 s; **highlights** ("10x token-efficient extracts"); `output_schema` structured outputs; category indexes | proprietary | Validates neural ranking + token-efficiency as the value axes; cloud-only |

### 2.2 Readers / extractors

| Project | What it does | How | License | Take |
|---|---|---|---|---|
| **Jina Reader** | URL→LLM-friendly content | `r.jina.ai/<url>`; picks intelligently between **headless Chrome and curl-impersonate**; `x-respond-with` (markdown/html/text/screenshot/frontmatter), `x-with-generated-alt` (VLM image captions), `x-target-selector`/`x-wait-for-selector`, `x-token-budget`; PDF (PDF.js) + Office (LibreOffice); `s.jina.ai` = search + fetch top-5 full content | Apache-2.0 | The best header-config surface; self-hostable but it's a service, not a library |
| **trafilatura** | Main-content extraction | Custom rule-based extractor with **jusText + readability-lxml as fallbacks**; discards recurring boilerplate; metadata (title/author/date/site/tags); best-in-class benchmark record (ScrapingHub, Bevendorff et al. 2023); TXT/MD/JSON/XML-TEI out | Apache-2.0 (since v1.8) | The extraction **algorithm** to port to Rust — Apache-2.0 makes this clean |
| **Firecrawl** | Scrape/crawl/search API for LLMs | scrape / crawl / **map** (URL discovery) / search / interact / agent endpoints; markdown+HTML+screenshot+schema-JSON out; JS rendering + rotating proxies; P95 3.4 s claim | AGPL-3.0 core | Defines the modern endpoint set (`map` especially); cloud-first, AGPL core |
| **Crawl4AI** | Async Playwright crawler | `fit_markdown` via **PruningContentFilter + BM25 alternative**; schema-driven `JsonCssExtractionStrategy` (no LLM) vs `LLMExtractionStrategy`; deep crawling BFS/DFS/Best-First with `resume_state`; `prefetch` URL discovery (5–10× claim); identity-aware persistent browser profiles | Apache-2.0 | Deep-crawl + resume patterns to copy; Python/Playwright stack we will not ship |

### 2.3 Crawlers + browser automation

- **spider-rs** (MIT): concurrency-first Rust crawler; **streaming** results via subscribe channels; `crawl_smart()` goes HTTP-first and **escalates to headless Chrome only on pages that need it**; `with_delay` + `with_respect_robots_txt`; WARC/MD/JSON export; MCP server + distributed workers. The right Rust foundation pattern — we may depend on it directly or borrow its escalation design.
- **Playwright MCP** (Apache-2.0): agents drive browsers via **accessibility-tree snapshots, not pixels** — deterministic element refs, no vision model. Snapshot mode default, vision mode opt-in.
- **Stagehand** (MIT): three primitives — `act()` / `extract()` / `observe()` — Playwright + LLM with **self-healing** when sites change.

### 2.4 Agent-web standards (the new rules of the road)

- **llms.txt v2** (verified at llmstxt.org): H1 + blockquote summary + H2 file-lists of `[name](url): notes`, with an `Optional` section convention. Served at `/llms.txt` **or any subpath** (most-specific wins; RFC 8615 well-known was explicitly rejected). Discovery via link relations: `rel="alternate" type="text/markdown"` for the markdown version of a page, `rel="describedby"` for the covering llms.txt. Thousands of sites publish one (OpenAI, Anthropic, Gemini docs; Mintlify/GitBook/Wix/Yoast generate them; Lighthouse audits for it).
- **Markdown content negotiation** (verified via Cloudflare's "Markdown for Agents"): send `Accept: text/markdown` → edge converts HTML→markdown on the fly; response carries `vary: accept`, `x-markdown-tokens` (token count for context planning), and **`Content-Signal: ai-train=…, search=…, ai-input=…`** — a machine-readable usage-permission header. Claude Code and OpenCode already send the Accept header.
- **robots.txt + ai.txt / Content-Signal**: classic robots is table stakes; the modern layer is per-use permissions (train vs search vs input).

## 3. Gap analysis → what we build

Nobody in the open-source field combines all of: keyless operation, neural (local) ranking, llms.txt/content-negotiation awareness, provenance/auditability, and kernel-grade politeness. Each project has two or three; the tool that has all five does not exist. That is the build target.

## 4. Target architecture

Five layers over the existing kernel. Rust (`nct-net` crate) is primary; oracle mjs mirrors for conformance; golden shape + committed HTML fixtures make tests deterministic offline; live tests behind `RUN_LIVE=1` (spider's pattern).

```
L4  INTELLIGENCE   net.research   net.watch   net.verify          (composite, semantic)
L3  SITE           net.map        net.crawl   net.extract         (multi-page, structured)
L2  SEARCH         net.search     (federated engines + RRF + local rerank)
L1  READ           net.fetch      net.robots  (extract/markdown, cache, llms.txt-aware)
L0  TRANSPORT      net.http (hardened)      net.probePort       (exists)
    under: politeness engine · cache/journal · SSRF guard · embedder (nct-semantic)
```

### L0 — Transport (harden `net.http`, no semantics change)
Keep raw-curl semantics (benchmark harness legitimately probes localhost). Add: response size cap option, charset handling, optional `blockPrivate` flag, redirect chain reporting.

### L1 — Read
- **`net.fetch`** — `{url, engine: "auto"|"http"|"render", selector?, maxTokens?} → {markdown, title, author?, date?, siteName?, links[], tokens, contentSignal, provenance{fetchedAt, hash, cache: "hit"|"miss"|"revalidated"}}`
  Pipeline, in order: (1) **markdown-native probe** — `Accept: text/markdown` negotiation, `.md` URL variant, page's `rel="alternate" type="text/markdown"` link, site `llms.txt` (llms.txt-first: when the site publishes curated markdown, prefer it over extraction); (2) **HTTP + extract** — trafilatura-style rule-based extraction in Rust (DOM via `scraper`/`lol_html`), with a readability-style fallback and a **consensus confidence score** (agreement between the two extractors); (3) **render escalation** — JS-detected (near-empty main content) → system-browser headless (`msedge/chrome --headless --dump-dom --virtual-time-budget`), zero new dependencies on Windows; spider's HTTP-first-escalate pattern.
  Security: **SSRF guard default-on** (block loopback/private/link-local + re-validate every redirect hop; `allowPrivate: true` to override), content-type allowlist, size cap.
  Headers we send: honest UA (`nc-tools/1.0 (+agent)`), `Accept: text/markdown`. Headers we surface: `Content-Signal` parsed into the result, `x-markdown-tokens` equivalent computed locally.
- **`net.robots`** — `{url} → {allowed, crawledVersion, llmsTxt?, contentSignals?, sitemaps[]}`: robots.txt parse + isAllowed per UA group, llms.txt discovery via `rel="describedby"` + root probe, sitemap autodiscovery.
- **Cache (not a tool, kernel service)**: file/SQLite store under the workspace cache dir; URL+content-hash keyed; TTL + **ETag/If-Modified-Since revalidation**; every fetch **journaled** (auditability none of the MCP fetch servers have).

### L2 — Search
- **`net.search`** — `{query, engines?: "auto"|list, region?, freshness?: "d"|"w"|"m", maxResults, rerank?: bool} → {results: [{title, url, snippet, engine, score, rank}], fusion: "rrf", reranked: bool, latencyMs}`
  Keyless engine set (all verified scrapeable/known-good by ddgs' existence): DDG-lite, Mojeek, Wikipedia, Marginalia, HN (Algolia API). Keyed adapters behind config: Brave, Tavily, Exa (never required). Pipeline: parallel engine fan-out with per-engine timeouts → **RRF fusion** → URL normalization + simhash dedupe → **local MiniLM rerank** (`nct-semantic`, query-vs-snippet+title cosine — pure Rust, offline) → politeness-aware.
  Engines defined as **data** (adapter descriptors: endpoint template, selectors/JSON path, rate limit), not hardcoded branches — ddgs breaks regularly when a backend changes HTML; data-defined adapters are patchable without a release.

### L3 — Site
- **`net.map`** — `{url, strategy: "llms-txt"|"sitemap"|"crawl", maxPages} → {urls: [{url, title?}], source}`. llms.txt is a curated site map — try it first, then sitemap.xml, then budgeted BFS (Firecrawl `map` + Crawl4AI `prefetch` pattern).
- **`net.crawl`** — `{url|urls, maxPages, maxDepth, sameDomain, respectRobots: true, delayMs} → streamed {page: {url, markdown, hash, links}, progress}`. Budget-bounded BFS, incremental via cache/conditional GET, **near-dup suppression** (simhash, optionally embedder cosine), robots-aware per-host delays.
- **`net.extract`** — `{url|markdown, schema?: css-selectors|json-ld|opengraph|microdata|feed} → structured JSON`. JSON-LD/OG/microdata/feed autodiscovery is no-LLM structured extraction (Crawl4AI `JsonCssExtractionStrategy` analog).

### L4 — Intelligence (the inventions, below)

## 5. The inventions — what "surpasses" concretely

1. **Keyless neural metasearch.** RRF fusion over free engines + local MiniLM rerank. ddgs has fusion without ranking; Exa has ranking behind a key and a cloud; SearXNG has neither. We get Exa-grade ordering with zero keys, zero cloud, zero cost — our embedder already runs offline in pure Rust.
2. **llms.txt-native fetching.** First tool chain that *prefers* the site's curated markdown (llms.txt → `rel="alternate"` → `Accept: text/markdown`) before falling back to extraction. The standard is verified to have thousands of sites; no surveyed tool pipeline exploits it as a first-class fast path.
3. **Consensus extraction with confidence.** Run the rule-based extractor and the readability-style fallback, score their agreement, emit `extractionConfidence` + fall back to render escalation when it's low. trafilatura falls back internally but emits no confidence signal the agent can act on.
4. **Provenance-grade fetch.** Every fetch journaled with URL, timestamp, content hash, cache state, content signals — an agent's citations become mechanically checkable. No surveyed MCP/web tool does this.
5. **`net.verify` — the anti-hallucination tool.** `{claim, url}` → re-fetch, semantic-match the claim against page passages (embedder), return `grounded | not-grounded | partial` with the grounding span. Nobody ships grounding-check as a tool; it closes the loop on agent citations.
6. **`net.research` — deep-research primitive without an LLM.** query → federated search → rerank → top-N fetch+extract → chunk → embed → **extractive evidence pack** (ranked passages, each with source URL + span) → the calling agent synthesizes from grounded material. Perplexica minus the LLM/SearXNG dependencies; Exa's highlights minus the cloud.
7. **Zero-dependency render escalation** via the system browser's headless mode (msedge is preinstalled on Windows) — JS pages handled without shipping Chromium or Playwright.
8. **Exemplar as well as consumer**: our own node's self-describing endpoints gain `/llms.txt` + `Accept: text/markdown` on API docs + `x-markdown-tokens` — the node that reads the agent web should also be readable by it (extends the self-describing-endpoints work).

## 6. Legal / licensing strategy

- **Apache-2.0** (trafilatura, Jina Reader, Crawl4AI, Playwright MCP): port algorithms/ideas freely; attribution.
- **MIT** (ddgs, spider-rs, Stagehand, Perplexica): direct dependency allowed (spider-rs is a cargo crate — candidate for `net.crawl` internals or escalation design inspiration).
- **AGPL-3.0** (SearXNG, Firecrawl): no code copying; reimplement ideas from spec/behavior; optional SearXNG integration only as an external service the user points us at.
- All engine adapters implemented from wire observation (protocols/papers, not scraped code).

## 7. Wave plan with machine-proven acceptance

### Wave W-Net-1: keyless core
`net.fetch` (+extractor, SSRF, cache, journal), `net.robots`, `net.search` (5 keyless engines + RRF + rerank), `net.http` hardening. Oracle parity, golden shape tests, committed HTML **fixtures** → deterministic offline extraction tests; live smoke behind `RUN_LIVE=1`.
**Verifier arms:** extraction token-F1 vs hand-gold on fixtures (target ≥0.90 static pages); search precision@10 on a fixed 15-query set with logged runs; rerank lift = top-3 hit-rate delta vs raw engine order (must be > 0 on the gold subset); SSRF: private-URL attempts must fail closed; untouched-run verifier must fail (proves the verifier bites).

### Wave W-Net-2: site intelligence
`net.map`, `net.crawl`, `net.extract` (JSON-LD/OG/feeds), render escalation, Brave/Tavily/Exa keyed adapters, `Content-Signal` parsing surfaced in fetch results.
**Verifier arms:** ladder — easy: static docs page; medium: JS-shell page (render must engage); hard: blocked/403 site → honest structured failure + hint (no silent garbage); expert: 3-hop crawl with robots-restricted path excluded.

### Wave W-Net-3: frontier
`net.research`, `net.watch` (change detection + semantic diff), `net.verify`, own-node `/llms.txt` + markdown negotiation serving.
**Verifier arms:** `net.verify` planted-claim test (true claim → grounded; mutated claim → rejected); `net.research` citation-coverage score on 5 research questions; `net.watch` on fixture-mutated cached page.

## 8. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Keyless engine HTML changes (ddgs breaks regularly) | Engines as data descriptors; multi-engine fusion degrades gracefully; fixture-based engine tests |
| IP blocks / CAPTCHAs | Politeness engine (per-host token bucket, persisted), honest UA, robots + Content-Signal compliance, structured failures |
| Extraction quality variance | Consensus scoring + render escalation; benchmark suite gates regressions |
| SSRF / abuse | Default-on private-range block with redirect re-validation; explicit `allowPrivate` opt-in; journaled requests |
| Scope creep (browser automation) | Browser tier stays out of scope; `net.render` headless-dump covers JS pages; full automation is a separate future wave |

## 9. Sources (fetched & verified 2026-09-01)

ddgs README · SearXNG docs · Perplexica/Vane README · Tavily docs index · Exa search API guide · Firecrawl README · Jina Reader README · trafilatura README · spider-rs/spider README · Crawl4AI README · llmstxt.org (v2) · Cloudflare "Markdown for Agents" blog · microsoft/playwright-mcp README · browserbase/stagehand README. The proposed `/.well-known/markdown` IETF draft could not be located (404) — content negotiation per Cloudflare + llms.txt v2 link-relations cover the standardization story.
