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
    const call = async (path, init) => {
      if (draft) init = { ...init, headers: { ...init?.headers, 'X-Mis-Draft': draft } };
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
    // Set by the apps listener when an owner or editor tries the app's draft.
    const draft = (new RegExp(`(?:^|; )mis-draft-${slug}=(\\d+)`).exec(document.cookie || '') || [])[1];
    globalThis.mis = front(Promise.resolve(client(`/a/${slug}/api/v1/`, draft)));
    if (draft) draftBadge(slug, draft);
    feedback(slug, draft);
  }

  // `fn` once the page's body exists.
  function onBody(fn) {
    if (document.body) fn(); else document.addEventListener('DOMContentLoaded', fn);
  }
  const css = (el, style) => Object.assign(el.style, style);
  const pill = { position: 'fixed', zIndex: 2147483646, padding: '6px 12px', border: '0', borderRadius: '999px',
    font: '600 13px/1.4 system-ui, sans-serif', boxShadow: '0 1px 4px rgba(0,0,0,.3)' };

  // Owners and editors get a Feedback button: tap what to change (or draw around it), say what,
  // and it goes to an agent. No screenshot: the agent reproduces the view from where it was.
  function feedback(slug, draft) {
    onBody(async () => {
      const me = await globalThis.mis.me().catch(() => null);
      if (!me || !['owner', 'editor'].includes(me.role)) return;
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.textContent = 'Feedback';
      css(btn, { ...pill, right: '8px', bottom: 'calc(8px + env(safe-area-inset-bottom))', background: '#5EE6F2', color: '#062026' });
      btn.onclick = () => mark(btn);
      document.body.append(btn);
    });

    // A veil over the page: a tap marks the element under it, a drag the area drawn.
    function mark(btn) {
      btn.hidden = true;
      const veil = document.createElement('div'), ring = document.createElement('div'), hint = document.createElement('div');
      css(veil, { position: 'fixed', inset: '0', zIndex: 2147483647, cursor: 'crosshair', background: 'rgba(0,0,0,.12)', touchAction: 'none' });
      css(ring, { position: 'fixed', display: 'none', border: '3px solid #F2B05E', borderRadius: '50%', pointerEvents: 'none' });
      hint.textContent = 'Tap what to change, or draw around it';
      css(hint, { ...pill, left: '50%', top: '12px', transform: 'translateX(-50%)', background: '#111', color: '#fff', whiteSpace: 'nowrap' });
      veil.append(ring, hint);
      document.body.append(veil);
      const show = (r, round) => css(ring, { display: 'block', left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px`, borderRadius: round ? '50%' : '8px' });
      let from = null;
      veil.onpointerdown = (e) => { from = [e.clientX, e.clientY]; veil.setPointerCapture(e.pointerId); };
      veil.onpointermove = (e) => { if (from) show(box(from, [e.clientX, e.clientY]), true); };
      veil.onpointercancel = () => { from = null; ring.style.display = 'none'; };
      veil.onpointerup = (e) => {
        if (!from) return;
        const drawn = box(from, [e.clientX, e.clientY]), tap = drawn.w < 12 && drawn.h < 12;
        from = null;
        veil.style.pointerEvents = 'none'; // so the page, not the veil, is under the point
        const el = document.elementFromPoint(tap ? e.clientX : drawn.x + drawn.w / 2, tap ? e.clientY : drawn.y + drawn.h / 2);
        veil.style.pointerEvents = '';
        const b = tap && el && el.getBoundingClientRect();
        const rect = b ? { x: b.x, y: b.y, w: b.width, h: b.height } : drawn;
        show(rect, !tap);
        hint.remove();
        veil.onpointerdown = veil.onpointermove = veil.onpointerup = veil.onpointercancel = null;
        // Page coordinates, so a scrolled view is found again.
        const page = { ...rect, x: rect.x + scrollX, y: rect.y + scrollY };
        ask(veil, btn, { selector: el && el !== document.body ? selectorOf(el) : '', rect: page });
      };
    }

    // Say what to change, then send it.
    function ask(veil, btn, marked) {
      const panel = document.createElement('form'), note = document.createElement('textarea');
      const send = document.createElement('button'), cancel = document.createElement('button'), said = document.createElement('div');
      note.placeholder = 'What should change here?';
      note.rows = 3;
      css(note, { width: '100%', boxSizing: 'border-box', font: '16px/1.4 system-ui, sans-serif', padding: '8px', borderRadius: '8px', border: '1px solid #444', background: '#000', color: '#fff' });
      send.textContent = 'Send';
      cancel.textContent = 'Cancel';
      cancel.type = 'button';
      css(send, { ...pill, position: 'static', background: '#5EE6F2', color: '#062026' });
      css(cancel, { ...pill, position: 'static', background: 'transparent', color: '#ccc', boxShadow: 'none' });
      css(said, { color: '#F2726B', font: '13px system-ui, sans-serif' });
      // At the top: a phone's keyboard comes up from the bottom.
      css(panel, { position: 'fixed', left: '8px', right: '8px', top: 'calc(8px + env(safe-area-inset-top))', zIndex: 2147483647,
        display: 'grid', gap: '8px', padding: '12px', borderRadius: '14px', background: '#16181d', boxShadow: '0 4px 16px rgba(0,0,0,.4)' });
      const row = document.createElement('div');
      css(row, { display: 'flex', gap: '8px', justifyContent: 'flex-end' });
      row.append(cancel, send);
      panel.append(note, said, row);
      document.body.append(panel);
      note.focus();
      const done = () => { veil.remove(); panel.remove(); btn.hidden = false; };
      cancel.onclick = done;
      panel.onsubmit = async (e) => {
        e.preventDefault();
        send.disabled = true;
        const path = location.pathname.slice(`/a/${slug}/`.length);
        const body = { route: `/${path}${location.search}${location.hash}`, ...marked, viewport: { w: innerWidth, h: innerHeight }, note: note.value };
        const headers = { 'Content-Type': 'application/json', ...(draft ? { 'X-Mis-Draft': draft } : {}) };
        const res = await fetch(`/a/${slug}/api/v1/feedback`, { method: 'POST', headers, body: JSON.stringify(body) }).catch(() => null);
        if (res && res.ok) { done(); toast('Feedback sent'); return; }
        said.textContent = res ? (await res.text()) || `error ${res.status}` : 'not sent: no connection';
        send.disabled = false;
      };
    }
  }

  function toast(text) {
    const t = document.createElement('div');
    t.textContent = text;
    css(t, { ...pill, left: '50%', top: '12px', transform: 'translateX(-50%)', background: '#111', color: '#fff', whiteSpace: 'nowrap' });
    document.body.append(t);
    setTimeout(() => t.remove(), 3000);
  }

  // The rectangle between two points.
  function box([x1, y1], [x2, y2]) {
    return { x: Math.min(x1, x2), y: Math.min(y1, y2), w: Math.abs(x2 - x1), h: Math.abs(y2 - y1) };
  }

  // A selector a test can find again: `main > section:nth-of-type(2) > div.chart`, or from an id.
  function selectorOf(el) {
    const parts = [];
    for (let n = el; n && n.nodeType === 1 && n !== document.body && parts.length < 8; n = n.parentElement) {
      if (n.id) { parts.unshift(`#${CSS.escape(n.id)}`); break; }
      const same = n.parentElement ? [...n.parentElement.children].filter((c) => c.tagName === n.tagName) : [];
      const cls = [...n.classList].slice(0, 2).map((c) => `.${CSS.escape(c)}`).join('');
      parts.unshift(n.tagName.toLowerCase() + cls + (same.length > 1 ? `:nth-of-type(${same.indexOf(n) + 1})` : ''));
    }
    return parts.join(' > ');
  }

  // Trying a draft (on a copy of the records): say so, with a way back to the live app.
  function draftBadge(slug, draft) {
    onBody(() => {
      const a = document.createElement('a');
      a.href = `/a/${slug}/?draft=off`;
      a.textContent = `Draft v${draft} · test data · exit`;
      css(a, { ...pill, left: '8px', bottom: 'calc(8px + env(safe-area-inset-bottom))', background: '#f5a524', color: '#000', textDecoration: 'none' });
      document.body.append(a);
    });
  }
})();
