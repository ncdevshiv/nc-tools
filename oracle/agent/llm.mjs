// OpenAI-compatible chat client. Works with any provider exposing
// /v1/chat/completions (proxyhub, OpenAI, OpenRouter, vLLM, ...).
export function makeChat({ baseURL, apiKey, model }) {
  if (!baseURL) throw new Error('NCTOOLS_LLM_BASEURL is required');
  if (!model) throw new Error('NCTOOLS_LLM_MODEL is required');
  const url = baseURL.replace(/\/$/, '') + '/chat/completions';

  return async function chat(body) {
    const payload = { ...body, model: body.model || model };
    const resp = await fetch(url, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        ...(apiKey ? { Authorization: `Bearer ${apiKey}` } : {}),
      },
      body: JSON.stringify(payload),
    });
    if (!resp.ok) {
      const text = await resp.text().catch(() => '');
      const err = new Error(`LLM HTTP ${resp.status}: ${text.slice(0, 300)}`);
      err.status = resp.status;
      throw err;
    }
    return await resp.json();
  };
}
