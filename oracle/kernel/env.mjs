// env.* — session environment management. Session values override the host
// environment for every subsequent proc.spawn / proc.start in this session.
import { ToolError } from './errors.mjs';

export function makeEnvTools(sessionEnv) {
  const validName = (name) => typeof name === 'string' && /^[A-Za-z_][A-Za-z0-9_]*$/.test(name);

  const set = ({ name, value }) => {
    if (!validName(name)) throw new ToolError('ERR_BAD_INPUT', 'env name must match [A-Za-z_][A-Za-z0-9_]*', { got: name });
    if (typeof value !== 'string') throw new ToolError('ERR_BAD_INPUT', 'env value must be a string');
    const previous = sessionEnv.get(name) ?? process.env[name] ?? null;
    sessionEnv.set(name, value);
    return { name, value, previous, source: 'session' };
  };

  const get = ({ name }) => {
    if (!validName(name)) throw new ToolError('ERR_BAD_INPUT', 'env name must match [A-Za-z_][A-Za-z0-9_]*', { got: name });
    if (sessionEnv.has(name)) return { name, value: sessionEnv.get(name), source: 'session' };
    if (name in process.env) return { name, value: process.env[name], source: 'host' };
    return { name, value: null, source: 'unset' };
  };

  const list = () => ({ session: Object.fromEntries(sessionEnv) });

  return {
    'env.get': { handler: get },
    'env.set': { handler: set },
    'env.list': { handler: list },
  };
}
