// Make It So apps SDK: who is asking and the app's records, for a page served at /a/<slug>/.
// <script src="/a/_/mis.js"></script> defines `mis`; every method returns a promise and rejects
// with a MisError ({status, message}, plus `current` on a 409 and `index` for a batch's op).
// Types: /a/_/mis.d.ts. With `?mock=1` in the page's URL it talks to an in-memory twin
// (mis-mock.js) instead, and `mis.mock` seeds it; `&role=viewer` sets the mock's role.
(() => {
  'use strict';

  class MisError extends Error {
    constructor(status, message, current, index) {
      super(message);
      this.name = 'MisError';
      this.status = status;
      if (current !== undefined) this.current = current;
      if (index !== undefined) this.index = index;
    }
  }

  // The HTTP API under `base` (`/a/<slug>/api/v1/`). GET and POST only. A page loaded as a
  // draft names it on every call, so it never reads or writes live data once the draft is gone.
  function client(base, draft) {
    const call = async (path, init = {}) => {
      if (draft) init = { ...init, headers: { ...init.headers, 'X-Mis-Draft': draft } };
      const res = await fetch(base + path, init);
      if (res.ok) return res.json();
      if (res.status === 409) {
        const { current, index } = await res.json();
        throw new MisError(409, 'conflict', current, index);
      }
      const text = (await res.text()) || res.statusText;
      // A batch names its refused op: `ops[2]: data is …`.
      const op = /^ops\[(\d+)\]: /.exec(text);
      throw new MisError(res.status, text, undefined, op ? Number(op[1]) : undefined);
    };
    const at = (space, collection, id) =>
      ['records', space, collection, id].filter((s) => s !== undefined).map(encodeURIComponent).join('/');
    const post = (body) =>
      call('mutations', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    const mutate = (op) => post(op).then((r) => r.record);
    return {
      me: () => call('me'),
      records: {
        list: (space, collection, q) =>
          call(at(space, collection) + (q ? `?q=${encodeURIComponent(JSON.stringify(q))}` : '')),
        // 200 null rather than a 404, which browsers log as an error.
        get: (space, collection, id) => call(`${at(space, collection, id)}?missing=null`),
        put: (space, collection, id, rev, data) => mutate({ op: 'put', space, collection, id, rev, data }),
        remove: (space, collection, id, rev) => mutate({ op: 'remove', space, collection, id, rev }),
        batch: (ops) => post({ ops }).then((r) => r.records),
      },
      export: () => call('export'),
    };
  }

  // `mis`, each method waiting for `ready` (the client, or the mock once loaded).
  function front(ready) {
    const via = (pick) => async (...args) => pick(await ready)(...args);
    return {
      MisError,
      me: via((m) => m.me),
      records: {
        list: via((m) => m.records.list),
        get: via((m) => m.records.get),
        put: via((m) => m.records.put),
        remove: via((m) => m.records.remove),
        batch: via((m) => m.records.batch),
      },
      export: via((m) => m.export),
    };
  }

  const params = new URLSearchParams(location.search);
  if (params.get('mock') === '1') {
    const src = new URL('mis-mock.js', document.currentScript.src);
    const ready = import(src).then((m) => m.createMock({ role: params.get('role') || 'owner', Err: MisError }));
    const via = (name) => async (...args) => (await ready)[name](...args);
    globalThis.mis = { ...front(ready), mock: { seed: via('seed'), setRole: via('setRole') } };
  } else {
    const slug = location.pathname.split('/')[2];
    // Set by the apps listener when its owner tries the app's draft.
    const draft = (new RegExp(`(?:^|; )mis-draft-${slug}=(\\d+)`).exec(document.cookie || '') || [])[1];
    globalThis.mis = front(Promise.resolve(client(`/a/${slug}/api/v1/`, draft)));
    if (draft) draftBadge(slug, draft);
  }

  // Trying a draft (on a copy of the records): say so, with a way back to the live app.
  function draftBadge(slug, draft) {
    const show = () => {
      const a = document.createElement('a');
      a.href = `/a/${slug}/?draft=off`;
      a.textContent = `Draft v${draft} · test data · exit`;
      Object.assign(a.style, { position: 'fixed', left: '8px', bottom: 'calc(8px + env(safe-area-inset-bottom))', zIndex: 2147483647,
        padding: '4px 10px', borderRadius: '999px', background: '#f5a524', color: '#000', font: '600 12px/1.5 system-ui, sans-serif',
        textDecoration: 'none', boxShadow: '0 1px 4px rgba(0,0,0,.3)' });
      document.body.append(a);
    };
    if (document.body) show(); else addEventListener('DOMContentLoaded', show);
  }
})();
