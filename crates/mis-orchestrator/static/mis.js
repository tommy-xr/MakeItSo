// Make It So apps SDK: who is asking and the app's records, for a page served at /a/<slug>/.
// <script src="/a/_/mis.js"></script> defines `mis`; every method returns a promise and rejects
// with a MisError ({status, message}, plus `current` on a 409). With `?mock=1` in the page's URL
// it talks to an in-memory twin (mis-mock.js) instead; `&role=viewer` sets the mock's role.
(() => {
  'use strict';

  class MisError extends Error {
    constructor(status, message, current) {
      super(message);
      this.name = 'MisError';
      this.status = status;
      if (current !== undefined) this.current = current;
    }
  }

  // The HTTP API under `base` (`/a/<slug>/api/v1/`). GET and POST only.
  function client(base) {
    const call = async (path, init) => {
      const res = await fetch(base + path, init);
      if (res.ok) return res.json();
      if (res.status === 409) throw new MisError(409, 'conflict', (await res.json()).current);
      throw new MisError(res.status, (await res.text()) || res.statusText);
    };
    const at = (space, collection, id) =>
      ['records', space, collection, id].filter((s) => s !== undefined).map(encodeURIComponent).join('/');
    const mutate = (op) =>
      call('mutations', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(op),
      }).then((r) => r.record);
    return {
      me: () => call('me'),
      records: {
        list: (space, collection, q) =>
          call(at(space, collection) + (q ? `?q=${encodeURIComponent(JSON.stringify(q))}` : '')),
        get: (space, collection, id) =>
          call(at(space, collection, id)).catch((e) => {
            if (e.status === 404) return null;
            throw e;
          }),
        put: (space, collection, id, rev, data) => mutate({ op: 'put', space, collection, id, rev, data }),
        remove: (space, collection, id, rev) => mutate({ op: 'remove', space, collection, id, rev }),
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
      },
      export: via((m) => m.export),
    };
  }

  const params = new URLSearchParams(location.search);
  if (params.get('mock') === '1') {
    const src = new URL('mis-mock.js', document.currentScript.src);
    const mock = import(src).then((m) => m.createMock({ role: params.get('role') || 'owner', Err: MisError }));
    globalThis.mis = { ...front(mock), mock };
  } else {
    const slug = location.pathname.split('/')[2];
    globalThis.mis = front(Promise.resolve(client(`/a/${slug}/api/v1/`)));
  }
})();
