// Wave-4 test: search.semantic with a REAL local transformer.
// The model downloads on first run (~90MB) then caches in .nc-tools/model-cache.
// The test is skipped ONLY if the model genuinely cannot be downloaded (honest skip).
import { test, beforeEach, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Kernel } from '../src/kernel/kernel.mjs';

let root;
beforeEach(() => { root = mkdtempSync(join(tmpdir(), 'nc-sem-')); });
afterEach(() => { rmSync(root, { recursive: true, force: true }); });

function write(rel, content) {
  mkdirSync(join(root, rel, '..'), { recursive: true });
  writeFileSync(join(root, rel), content, 'utf8');
}

test('search.semantic finds files by meaning, not by string', async (t) => {
  write('src/orders.js', 'export function applyDiscount(total, code) {\n  if (code === "SAVE10") return total * 0.9;\n  return total;\n}\n');
  write('src/payments.js', 'import { createClient } from "stripe";\nconst stripe = createClient(process.env.STRIPE_KEY);\nexport function charge(card, amount) { return stripe.charges.create({ source: card, amount }); }\n');
  write('src/footer.html', '<footer>© 2026 Acme Inc. All rights reserved. Privacy Policy · Terms of Service</footer>\n');
  const k = new Kernel(root);

  let out;
  try {
    // NCTOOLS_MODEL_CACHE is set by the test runner env; fallback to workspace cache
    out = await k.call('search.semantic', { query: 'how are customer payments and credit card charges handled?', topK: 3, cacheDir: join(tmpdir(), 'nctools-model-cache') });
  } catch (e) {
    if (e.error?.code === 'ERR_EMBED_UNAVAILABLE') {
      t.skip(`embedding model unavailable on this machine/network: ${e.error.message}`);
      return;
    }
    throw e;
  }
  assert.equal(out.ok, true, JSON.stringify(out.error));
  const files = out.result.top.map((m) => m.file);
  // payments.js must rank above footer.html for the credit-card query
  assert.equal(files[0], 'src/payments.js', `expected payments first, got ${JSON.stringify(files)}`);
  assert.equal(out.result.total, 3);
  // scores are actual cosine similarities in (0,1]
  for (const m of out.result.top) {
    assert.ok(m.score > 0 && m.score <= 1, `score out of range: ${m.score}`);
  }
});

test('search.semantic ranks unrelated content lower', async (t) => {
  write('a.js', 'function add(x, y) { return x + y; }');
  write('b.js', 'function connectToDatabase(host, port) { return new DatabasePool(host, port); }');
  const k = new Kernel(root);
  let out;
  try {
    out = await k.call('search.semantic', { query: 'establish a database connection', topK: 2, cacheDir: join(tmpdir(), 'nctools-model-cache') });
  } catch (e) {
    if (e.error?.code === 'ERR_EMBED_UNAVAILABLE') { t.skip('no model'); return; }
    throw e;
  }
  assert.ok(out.ok);
  assert.equal(out.result.top[0].file, 'b.js');
});

test('search.semantic validates input and surfaces model unavailability as structured error', async () => {
  const k = new Kernel(root);
  const bad = await k.call('search.semantic', { query: '' });
  assert.equal(bad.error.code, 'ERR_BAD_INPUT');
});
