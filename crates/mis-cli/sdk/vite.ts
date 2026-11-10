// Vite plugin for an app at apps/<slug>/: served at /a/<slug>/ and the SDK at /a/_/, like on the
// apps listener. `plugins: [react(), misApp('<slug>')]`.
// MIS_APPS_URL set (e.g. http://127.0.0.1:7475): dev and preview proxy the SDK and the app's API
//   to that local orchestrator; MIS_DEV_ROLE=viewer|editor tries a lesser role (orchestrator --apps-dev).
// Unset: they serve this folder's SDK for /a/<slug>/?mock=1; no orchestrator.
// Types are structural: vite resolves from the app's node_modules, not from here.
import { readFileSync } from 'node:fs';
import type { IncomingMessage, ServerResponse } from 'node:http';
import { fileURLToPath } from 'node:url';

// The apps listener's CSP, so `vite preview` (and shots) catch what it would block. One line: a
// MakeItSo test checks it is the listener's.
const CSP = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

const SDK: Record<string, string> = { 'mis.js': 'text/javascript', 'mis-mock.js': 'text/javascript', 'mis.d.ts': 'text/plain' };

type Handler = (req: IncomingMessage, res: ServerResponse, next: () => void) => void;
type Server = { middlewares: { use(fn: Handler): unknown } };
type ProxyReq = { setHeader(name: string, value: string): void };

export function misApp(slug: string) {
  const target = process.env.MIS_APPS_URL;
  const devRole = process.env.MIS_DEV_ROLE;
  // Writes must come from the listener's own origin, so the proxy presents it.
  const api = {
    target,
    changeOrigin: true,
    configure: (proxy: { on(event: 'proxyReq', fn: (req: ProxyReq) => void): void }) =>
      proxy.on('proxyReq', (req) => {
        req.setHeader('origin', new URL(target!).origin);
        if (devRole) req.setHeader('x-mis-dev-role', devRole);
      }),
  };
  const proxy = target ? { '/a/_': { target, changeOrigin: true }, [`/a/${slug}/api`]: api } : undefined;

  const serve: Handler = (req, res, next) => {
    const name = req.url?.split('?')[0].replace(/^\/a\/_\//, '');
    if (!name || !Object.hasOwn(SDK, name)) return next();
    res.setHeader('content-type', `${SDK[name]}; charset=utf-8`);
    res.end(readFileSync(new URL(`./${name}`, import.meta.url)));
  };
  const local = (s: Server) => {
    if (!target) s.middlewares.use(serve);
  };

  return {
    name: 'mis-app',
    config: () => ({
      base: `/a/${slug}/`,
      // The app and this folder's helpers (only the app's own folder by default).
      server: { proxy, fs: { allow: [process.cwd(), fileURLToPath(new URL('.', import.meta.url))] } },
      preview: { proxy, headers: { 'Content-Security-Policy': CSP } },
    }),
    configureServer: local,
    configurePreviewServer: local,
  };
}
