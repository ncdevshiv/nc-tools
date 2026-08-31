// Child-process environment normalization. Windows launchers (Electron/bun
// apps, shell shims) sometimes carry a minimal 'PATH' next to the full
// case-variant 'Path'; spawn resolution then hides git/node from kernels.
// Hand children the full 'Path' when it is strictly longer.
export function childEnv(extra = {}) {
  const env = { ...process.env, ...extra };
  if (process.platform === 'win32' && typeof env.Path === 'string' && typeof env.PATH === 'string' && env.Path.length > env.PATH.length) {
    env.PATH = env.Path;
  }
  return env;
}
