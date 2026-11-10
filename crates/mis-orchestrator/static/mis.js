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
      menu: { mount: (el) => menu.mount(el) },
    };
  }

  const params = new URLSearchParams(location.search);
  const mocked = params.get('mock') === '1';
  const slug = location.pathname.split('/')[2];
  // Set by the apps listener when an owner or editor tries the app's draft.
  const draft = mocked ? undefined : (new RegExp(`(?:^|; )mis-draft-${slug}=(\\d+)`).exec(document.cookie || '') || [])[1];
  const menu = platformMenu();
  if (mocked) {
    const src = new URL('mis-mock.js', document.currentScript.src);
    const ready = import(src).then((m) => m.createMock({ role: params.get('role') || 'owner', Err: MisError }));
    const via = (name) => async (...args) => (await ready)[name](...args);
    globalThis.mis = { ...front(ready), mock: { seed: via('seed'), setRole: via('setRole') } };
  } else {
    globalThis.mis = front(Promise.resolve(client(`/a/${slug}/api/v1/`, draft)));
  }

  // `fn` once the page's body exists.
  function onBody(fn) {
    if (document.body) fn(); else document.addEventListener('DOMContentLoaded', fn);
  }
  const make = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };

  // Neutral, following light and dark; an app themes it with --mis-menu-fg/-bg (the button)
  // and --mis-menu-surface/-text (the menu, the feedback panel).
  const STYLE = `
.mis-ui{--s:var(--mis-menu-surface,#fff);--t:var(--mis-menu-text,#1c1c1e);--d:color-mix(in srgb,var(--t) 60%,transparent);--l:color-mix(in srgb,var(--t) 12%,transparent)}
@media (prefers-color-scheme:dark){.mis-ui{--s:var(--mis-menu-surface,#1c1c1e);--t:var(--mis-menu-text,#f2f2f7)}}
.mis-mb{all:unset;box-sizing:border-box;position:relative;display:inline-flex;align-items:center;justify-content:center;width:32px;height:32px;border-radius:8px;cursor:pointer;color:var(--mis-menu-fg,currentColor);background:var(--mis-menu-bg,transparent);opacity:.7;font:600 18px/1 system-ui,sans-serif;letter-spacing:1px}
.mis-mb:hover,.mis-mb[aria-expanded=true]{opacity:1}
.mis-mb[hidden]{display:none}
.mis-mb:focus-visible{outline:2px solid currentColor}
.mis-mb.mis-float{position:fixed;top:calc(8px + env(safe-area-inset-top));right:calc(8px + env(safe-area-inset-right));z-index:2147483646;color:var(--mis-menu-fg,var(--t));background:var(--mis-menu-bg,color-mix(in srgb,var(--s) 70%,transparent));-webkit-backdrop-filter:blur(8px);backdrop-filter:blur(8px)}
.mis-mb .mis-dot{position:absolute;top:3px;right:2px;width:8px;height:8px;border-radius:50%;background:var(--mis-menu-dot,#e8a33d)}
.mis-pop{position:fixed;z-index:2147483647;box-sizing:border-box;width:min(300px,calc(100vw - 16px));overflow-y:auto;padding:6px;border-radius:12px;background:var(--s);color:var(--t);box-shadow:0 0 0 1px var(--l),0 8px 24px rgba(0,0,0,.25);font:14px/1.4 system-ui,sans-serif}
.mis-pop a,.mis-pop button{all:unset;box-sizing:border-box;display:block;width:100%;padding:9px 10px;border-radius:8px;cursor:pointer;color:var(--t)}
.mis-pop a:hover,.mis-pop button:hover,.mis-pop a:focus-visible,.mis-pop button:focus-visible{background:var(--l)}
.mis-pop .mis-dim{padding:6px 10px;color:var(--d);font-size:12px}
.mis-pop .mis-fb{padding:4px 10px;font-size:13px}
.mis-pop .mis-fb span{display:block;color:var(--d);font-size:12px}
.mis-pop hr{border:0;border-top:1px solid var(--l);margin:4px 2px}
.mis-veil{position:fixed;inset:0;z-index:2147483646;cursor:crosshair;background:rgba(0,0,0,.12);touch-action:none}
.mis-ring{position:fixed;display:none;pointer-events:none;border:2px solid #fff;box-shadow:0 0 0 2px rgba(0,0,0,.55)}
.mis-hint{position:fixed;left:50%;top:calc(10px + env(safe-area-inset-top));transform:translateX(-50%);padding:6px 12px;border-radius:999px;white-space:nowrap;background:var(--s);color:var(--t);box-shadow:0 0 0 1px var(--l);font:13px/1.4 system-ui,sans-serif;z-index:2147483647}
.mis-panel{position:fixed;left:8px;right:8px;top:calc(8px + env(safe-area-inset-top));z-index:2147483647;display:grid;gap:8px;padding:12px;border-radius:14px;background:var(--s);color:var(--t);box-shadow:0 0 0 1px var(--l),0 8px 24px rgba(0,0,0,.25);font:14px/1.4 system-ui,sans-serif}
.mis-panel textarea{box-sizing:border-box;width:100%;padding:8px;border:1px solid var(--l);border-radius:8px;background:transparent;color:inherit;font:16px/1.4 system-ui,sans-serif}
.mis-panel .mis-row{display:flex;gap:8px;justify-content:flex-end}
.mis-panel button{padding:7px 14px;border:1px solid var(--l);border-radius:999px;background:transparent;color:inherit;font:600 13px/1.4 system-ui,sans-serif}
.mis-panel button[type=submit]{background:var(--t);color:var(--s);border-color:var(--t)}
.mis-panel .mis-err{color:#d33;font-size:13px}
.mis-tag{position:fixed;left:calc(8px + env(safe-area-inset-left));bottom:calc(8px + env(safe-area-inset-bottom));z-index:2147483646;padding:4px 10px;border-radius:999px;background:color-mix(in srgb,var(--s) 80%,transparent);color:var(--t);box-shadow:0 0 0 1px var(--mis-menu-dot,#e8a33d);font:12px/1.5 system-ui,sans-serif;text-decoration:none;-webkit-backdrop-filter:blur(8px);backdrop-filter:blur(8px)}`;

  // The platform menu: who's signed in, All apps, and for owners and editors the app's draft,
  // what became of their feedback, and Give feedback. Apps place its button in their header
  // (`mis.menu.mount(el)`); else it floats top-right.
  function platformMenu() {
    let btn = null, host = null, me = null, pop = null;
    const mount = (el) => {
      if (!el) return; // a React ref also calls with null
      host = el;
      if (btn) { btn.classList.remove('mis-float', 'mis-ui'); el.append(btn); }
    };
    const start = async () => {
      const style = make('style');
      style.textContent = STYLE;
      document.head.append(style);
      // Said whatever /me says: writes here go to test data.
      if (draft) {
        const tag = make('a', 'mis-tag mis-ui', `Draft v${draft} · test data · exit`);
        tag.href = `/a/${slug}/?draft=off`;
        document.body.append(tag);
      }
      me = await globalThis.mis.me().catch(() => null);
      if (!me) return;
      btn = make('button', 'mis-mb', '⋯');
      btn.type = 'button';
      btn.setAttribute('aria-label', 'App menu');
      btn.setAttribute('aria-expanded', 'false');
      const dot = make('span', 'mis-dot');
      const showDot = () => { if (me.drafted && !draft) btn.append(dot); else dot.remove(); };
      showDot();
      // Fresh each time it opens: a draft or a feedback status may have changed. A tap while
      // open closes it.
      btn.onclick = async () => {
        if (pop) return pop.close();
        me = (await globalThis.mis.me().catch(() => null)) || me;
        showDot();
        if (!pop) open();
      };
      if (host) return mount(host);
      // Not placed by the app: float, unless it places it soon after loading.
      setTimeout(() => { if (!host) { btn.classList.add('mis-float', 'mis-ui'); document.body.append(btn); } }, 800);
    };
    function open() {
      const el = make('div', 'mis-pop mis-ui');
      el.setAttribute('role', 'menu');
      const close = () => { el.remove(); pop = null; btn.setAttribute('aria-expanded', 'false'); removeEventListener('pointerdown', outside, true); };
      const outside = (e) => { if (!el.contains(e.target) && !btn.contains(e.target)) close(); };
      pop = { close };
      const item = (text, act) => {
        const a = make(typeof act === 'string' ? 'a' : 'button', '', text);
        if (typeof act === 'string') a.href = act; else a.onclick = () => { close(); act(); };
        a.setAttribute('role', 'menuitem');
        el.append(a);
      };
      const writes = me.role === 'owner' || me.role === 'editor';
      el.append(make('div', 'mis-dim', `Signed in as ${me.user} · ${me.role}`));
      if (draft) item(`Trying draft v${draft} · back to live`, `/a/${slug}/?draft=off`);
      else if (me.drafted) item(`Draft v${me.drafted} ready · Try it`, `/a/${slug}/?draft=${me.drafted}`);
      if (writes && !mocked) {
        for (const f of me.feedback || []) {
          const row = make('div', 'mis-fb', f.note);
          const state = f.state === 'draft_ready' ? `draft v${f.release} ready` : f.state === 'working' ? 'an agent is on it'
            : f.state === 'done' ? f.what : `waiting: ${f.why}`;
          row.append(make('span', '', state));
          el.append(row);
        }
        item('Give feedback…', () => mark());
      }
      el.append(make('hr'));
      item('All apps', '/a/');
      const r = btn.getBoundingClientRect();
      // Under the button, inside the screen: it scrolls rather than run past the bottom.
      const top = Math.min(r.bottom + 6, innerHeight - 120);
      Object.assign(el.style, { top: `${top}px`, maxHeight: `${innerHeight - top - 8}px` });
      document.body.append(el);
      const w = el.offsetWidth;
      el.style.left = `${Math.max(8, Math.min(r.right - w, innerWidth - w - 8))}px`;
      btn.setAttribute('aria-expanded', 'true');
      addEventListener('pointerdown', outside, true);
    }
    // A veil over the page: a tap marks the element under it, a drag the area drawn.
    function mark() {
      btn.hidden = true;
      const veil = make('div', 'mis-veil mis-ui'), ring = make('div', 'mis-ring'), hint = make('div', 'mis-hint', 'Tap what to change, or draw around it');
      veil.append(ring, hint);
      document.body.append(veil);
      const show = (r, round) => Object.assign(ring.style, { display: 'block', left: `${r.x}px`, top: `${r.y}px`, width: `${r.w}px`, height: `${r.h}px`, borderRadius: round ? '50%' : '8px' });
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
        ask(veil, { selector: el && el !== document.body ? selectorOf(el) : '', rect: page });
      };
    }
    // Say what to change, then send it.
    function ask(veil, marked) {
      const panel = make('form', 'mis-panel mis-ui'), note = make('textarea'), said = make('div', 'mis-err');
      const row = make('div', 'mis-row'), cancel = make('button', '', 'Cancel'), send = make('button', '', 'Send');
      note.placeholder = 'What should change here?';
      note.rows = 3;
      cancel.type = 'button';
      send.type = 'submit';
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
        if (res && res.ok) {
          done();
          toast('Feedback sent');
          return;
        }
        said.textContent = res ? (await res.text()) || `error ${res.status}` : 'not sent: no connection';
        send.disabled = false;
      };
    }
    return { mount, start };
  }

  function toast(text) {
    const t = make('div', 'mis-hint mis-ui', text);
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

  // Last: everything it uses is defined by now.
  if (typeof document === 'object' && document.addEventListener) onBody(() => menu.start());
})();
