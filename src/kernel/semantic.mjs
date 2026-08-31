// search.semantic — neural-semantic search over the workspace.
// A real transformer (all-MiniLM-L6-v2, ~90MB, runs locally via
// @xenova/transformers) embeds files; queries are ranked by cosine
// similarity. No API dependency, no remote calls. This is the capability
// grep provably lacks: it finds "where is auth handled?" by meaning.
import { readFileSync, existsSync, statSync, mkdirSync, readdirSync, writeFileSync, lstatSync, realpathSync } from 'node:fs';
import { join, relative, extname } from 'node:path';
import { ToolError } from './errors.mjs';
import { inWorkspace, isInsidePath } from './paths.mjs';

const TEXT_EXT = new Set(['.js', '.mjs', '.cjs', '.ts', '.tsx', '.jsx', '.json', '.md', '.txt', '.css', '.html', '.py', '.rs', '.go', '.java', '.yml', '.yaml', '.toml', '.sh', '.c', '.h', '.cpp', '.hpp', '.sql', '']);
const EMBED_EXT = new Set([...TEXT_EXT]);

let pipelinePromise = null;

async function getEmbedder(cacheDir) {
  if (!pipelinePromise) {
    const tf = await import('@xenova/transformers');
    tf.env.cacheDir = cacheDir;
    tf.env.allowLocalModels = false;
    pipelinePromise = tf.pipeline('feature-extraction', 'Xenova/all-MiniLM-L6-v2');
  }
  return pipelinePromise;
}

function* walkTextFiles(root, dir, depth = 0) {
  if (depth > 12) return;
  for (const name of readdirSync(dir).sort()) {
    if (name === '.git' || name === 'node_modules' || name === '.nc-tools') continue;
    const full = join(dir, name);
    let st;
    try { st = lstatSync(full); } catch { continue; }
    if (st.isSymbolicLink()) continue; // never follow links (they may leave the workspace)
    if (st.isDirectory()) {
      // a junction may point outside the workspace; skip it
      try { if (!isInsidePath(root, realpathSync(full))) continue; } catch { continue; }
      yield* walkTextFiles(root, full, depth + 1);
    } else if (EMBED_EXT.has(extname(full).toLowerCase())) yield full;
  }
}

function normalize(v) {
  let norm = 0;
  for (const x of v) norm += x * x;
  norm = Math.sqrt(norm) || 1;
  return v.map((x) => x / norm);
}

export function makeSemanticTools(root) {
  // digest-based cache: (file, sha256) -> vector, persisted in .nc-tools/semantic-index.jsonl
  const indexFile = join(root, '.nc-tools', 'semantic-index.jsonl');
  let cache = new Map();
  if (existsSync(indexFile)) {
    for (const line of readFileSync(indexFile, 'utf8').split('\n').filter(Boolean)) {
      try {
        const rec = JSON.parse(line);
        cache.set(`${rec.file}::${rec.digest}`, rec.vector);
      } catch { /* skip corrupt line */ }
    }
  }

  const saveEntry = (entry) => {
    cache.set(`${entry.file}::${entry.digest}`, entry.vector);
    writeFileSync(indexFile, JSON.stringify(entry) + '\n', { flag: 'a', encoding: 'utf8' });
  };

  const semantic = async ({ query, path = '.', topK = 5, cacheDir }) => {
    if (typeof query !== 'string' || !query.trim()) {
      throw new ToolError('ERR_BAD_INPUT', 'query must be a non-empty string');
    }
    // Validate the path BEFORE touching the model: refusing an escape must not
    // require a local embedding pipeline to be up (and must not read outside).
    const base = inWorkspace(root, path);
    let embedder;
    try {
      embedder = await getEmbedder(process.env.NCTOOLS_MODEL_CACHE || cacheDir || join(root, '.nc-tools', 'model-cache'));
    } catch (e) {
      throw new ToolError('ERR_EMBED_UNAVAILABLE', `embedding model unavailable: ${e.message}`, {
        hint: 'run npm install @xenova/transformers and check network access to huggingface.co',
      });
    }
    const { createHash } = await import('node:crypto');
    const files = [];
    for (const f of walkTextFiles(root, base)) files.push(f);

    // encode query
    const qOut = await embedder(query, { pooling: 'mean', normalize: true });
    const qVec = Array.from(qOut.data);

    // encode files (cache by content digest)
    const scored = [];
    for (const f of files) {
      const content = readFileSync(f, 'utf8');
      if (content.includes('\u0000')) continue;
      const text = content.slice(0, 32_000); // model window bound
      const digest = createHash('sha256').update(content).digest('hex');
      let vec = cache.get(`${f}::${digest}`);
      if (!vec) {
        const fOut = await embedder(text, { pooling: 'mean', normalize: true });
        vec = normalize(Array.from(fOut.data));
        saveEntry({ file: f, digest, vector: vec, ts: new Date().toISOString() });
      }
      let sim = 0;
      for (let i = 0; i < qVec.length; i++) sim += qVec[i] * vec[i];
      scored.push({ file: relative(root, f).replaceAll('\\', '/'), score: Number(sim.toFixed(4)), bytes: content.length });
    }
    scored.sort((a, b) => b.score - a.score);
    return { query, top: topK > 0 ? scored.slice(0, topK) : scored, total: scored.length };
  };

  return { 'search.semantic': { handler: semantic } };
}
