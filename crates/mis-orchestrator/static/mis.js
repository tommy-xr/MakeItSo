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
    const list = via((m) => m.records.list), batch = via((m) => m.records.batch);
    const isConflict = (e) => e instanceof MisError && e.status === 409;
    return {
      MisError,
      me: via((m) => m.me),
      records: {
        list,
        get: via((m) => m.records.get),
        put: via((m) => m.records.put),
        remove: via((m) => m.records.remove),
        batch,
        // Every page of a query.
        all: async (space, collection, q) => {
          const out = [];
          let cursor = null;
          do {
            const page = await list(space, collection, cursor ? { ...q, cursor } : q);
            out.push(...page.records);
            cursor = page.cursor;
          } while (cursor);
          return out;
        },
        // Create-only puts, 100 to a batch. An id already there (a 409 naming its op) is
        // dropped and the rest sent again, so a re-import or a second phone writes nothing twice.
        createAll: async (space, collection, rows) => {
          let added = 0, present = 0;
          for (let i = 0; i < rows.length; i += 100) {
            let ops = rows.slice(i, i + 100).map(({ id, data }) => ({ op: 'put', space, collection, id, rev: 0, data }));
            while (ops.length) {
              try {
                added += (await batch(ops)).length;
                break;
              } catch (e) {
                if (!isConflict(e) || e.index === undefined) throw e;
                ops = ops.filter((_, j) => j !== e.index);
                present++;
              }
            }
          }
          return { added, present };
        },
      },
      export: via((m) => m.export),
      isConflict,
      errorText: (e) => e instanceof MisError && e.status === 403 ? 'You can only view this app.' : `Not saved: ${e?.message ?? e}`,
      menu: { mount: (el) => menu.mount(el) },
      shell: (opts) => shell(opts),
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

  // Neutral, following light and dark; an app themes it with --mis-menu-fg/-bg (the button),
  // --mis-menu-surface/-text (the menu, the feedback panel, the tab bar), --mis-menu-line
  // (borders) and --mis-menu-accent (the current tab). The shell's header takes the page's colours.
  const STYLE = `
.mis-ui{--s:var(--mis-menu-surface,#fff);--t:var(--mis-menu-text,#1c1c1e);--d:color-mix(in srgb,var(--t) 60%,transparent);--l:var(--mis-menu-line,color-mix(in srgb,var(--t) 12%,transparent));--a:var(--mis-menu-accent,var(--t))}
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
.mis-tag{position:fixed;left:calc(8px + env(safe-area-inset-left));bottom:calc(8px + env(safe-area-inset-bottom));z-index:2147483646;padding:4px 10px;border-radius:999px;background:color-mix(in srgb,var(--s) 80%,transparent);color:var(--t);box-shadow:0 0 0 1px var(--mis-menu-dot,#e8a33d);font:12px/1.5 system-ui,sans-serif;text-decoration:none;-webkit-backdrop-filter:blur(8px);backdrop-filter:blur(8px)}
.mis-pop a[aria-current=page]{font-weight:600}
.mis-head{box-sizing:border-box;display:flex;align-items:center;gap:10px;max-width:560px;margin:0 auto;padding:calc(14px + env(safe-area-inset-top)) calc(16px + env(safe-area-inset-right)) 6px calc(16px + env(safe-area-inset-left))}
.mis-head h1{flex:1;min-width:0;margin:0;font-size:28px;line-height:1.2;letter-spacing:-.01em;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mis-head .mis-back{padding-right:2px;color:inherit;font-size:30px;line-height:1;text-decoration:none}
.mis-head [hidden]{display:none}
.mis-badge{padding:2px 10px;border-radius:99px;background:var(--l);color:var(--d);font-size:13px;white-space:nowrap}
.mis-main{display:block;max-width:560px;margin:0 auto}
.mis-main.mis-tabbed{padding-bottom:calc(64px + env(safe-area-inset-bottom))}
.mis-tabs{position:fixed;left:0;right:0;bottom:0;z-index:5;display:flex;justify-content:center;padding:0 env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left);background:var(--s);border-top:1px solid var(--l)}
.mis-tabs[hidden]{display:none}
.mis-tabs a{flex:1;max-width:160px;min-height:56px;display:flex;align-items:center;justify-content:center;color:var(--d);font-size:15px;text-decoration:none}
.mis-tabs a[aria-current=page]{color:var(--a);font-weight:600;box-shadow:inset 0 3px 0 var(--a)}`;
  let styled = false;
  const style = () => {
    if (styled) return;
    styled = true;
    const el = make('style');
    el.textContent = STYLE;
    document.head.append(el);
  };

  // The platform menu: who's signed in, All apps, and for owners and editors the app's draft,
  // what became of their feedback, and Give feedback; above them the shell's menu sections.
  // Apps place its button in their header (`mis.menu.mount(el)`); else it floats top-right.
  function platformMenu() {
    let btn = null, host = null, me = null, pop = null, items = [], gotMe;
    const known = new Promise((r) => (gotMe = r));
    const mount = (el) => {
      if (!el) return; // a React ref also calls with null
      host = el;
      if (btn) { btn.classList.remove('mis-float', 'mis-ui'); el.append(btn); }
    };
    const start = async () => {
      style();
      // Said whatever /me says: writes here go to test data.
      if (draft) {
        const tag = make('a', 'mis-tag mis-ui', `Draft v${draft} · test data · exit`);
        tag.href = `/a/${slug}/?draft=off`;
        document.body.append(tag);
      }
      me = await globalThis.mis.me().catch(() => null);
      gotMe(me);
      if (!me && !items.length) return;
      btn = make('button', 'mis-mb', '⋯');
      btn.type = 'button';
      btn.setAttribute('aria-label', 'App menu');
      btn.setAttribute('aria-expanded', 'false');
      const dot = make('span', 'mis-dot');
      const showDot = () => { if (me?.drafted && !draft) btn.append(dot); else dot.remove(); };
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
      const item = (text, act, current) => {
        const a = make(typeof act === 'string' ? 'a' : 'button', '', text);
        if (typeof act === 'string') a.href = act;
        a.onclick = () => { close(); if (typeof act !== 'string') act(); };
        a.setAttribute('role', 'menuitem');
        if (current) a.setAttribute('aria-current', 'page');
        el.append(a);
      };
      for (const s of items) item(s.label, s.href, s.current);
      if (me) {
        if (items.length) el.append(make('hr'));
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
      }
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
    // `me`: /me once loaded (null if it failed). `items`: the shell's menu sections, shown on open.
    return { mount, start, me: known, set items(list) { items = list; } };
  }

  // Shell core, pure. The section a hash names: `#/log/for/ann` -> {id: 'log', path: ['log',
  // 'for', 'ann']}. Empty or unknown: the first section, the path kept for the app (`#/p/ann`).
  function route(hash, sections) {
    const path = hash.replace(/^#\/?/, '').split('/').filter(Boolean);
    return { id: (sections.find((x) => x.id === path[0]) || sections[0]).id, path };
  }
  // What the shell shows for options `s` on route `r`: the heading (the section's label; one
  // section: the title; `heading` overrides), a back link, the tabs (two or more `tab`
  // sections) and the menu's sections (the rest; none for one section).
  function layout(s, r) {
    const many = s.sections.length > 1;
    const link = (x) => ({ label: x.label, href: `#/${x.id}`, current: x.id === r.id });
    const tabbed = s.sections.filter((x) => x.tab).length > 1;
    return {
      heading: s.heading ?? (many ? s.sections.find((x) => x.id === r.id).label : s.title),
      back: s.back ?? null,
      tabs: tabbed ? s.sections.filter((x) => x.tab).map(link) : [],
      items: many ? s.sections.filter((x) => !(tabbed && x.tab)).map(link) : [],
    };
  }
  const sectioned = (list) => {
    const ok = Array.isArray(list) && list.length && list.every((x) => /^[A-Za-z0-9_-]+$/.test(x?.id) && typeof x.label === 'string');
    if (!ok) throw new TypeError('mis.shell: sections are [{id: [A-Za-z0-9_-]+, label, tab?}], at least one');
  };

  // The app's frame, appended to the body: a header (heading, View only for viewers, the ⋯
  // menu), `main` for the app to render into, and bottom tabs. Once per page.
  let framed = false;
  function shell(opts) {
    sectioned(opts?.sections);
    if (framed) throw new Error('mis.shell: once per page');
    framed = true;
    style();
    let s = { title: document.title, ...opts };
    const head = make('header', 'mis-head mis-ui'), back = make('a', 'mis-back', '‹'), h1 = make('h1');
    const badge = make('span', 'mis-badge', 'View only'), slot = make('span');
    const main = make('main', 'mis-main'), tabs = make('nav', 'mis-tabs mis-ui');
    back.setAttribute('aria-label', 'Back');
    badge.hidden = true;
    head.append(back, h1, badge, slot);
    document.body.append(head, main, tabs);
    const link = ({ label, href, current }) => {
      const a = make('a', '', label);
      a.href = href;
      if (current) a.setAttribute('aria-current', 'page');
      return a;
    };
    let r = route(location.hash, s.sections);
    const render = () => {
      const v = layout(s, r);
      h1.textContent = v.heading;
      back.hidden = !v.back;
      if (v.back) back.href = v.back;
      tabs.replaceChildren(...v.tabs.map(link));
      tabs.hidden = !v.tabs.length;
      main.classList.toggle('mis-tabbed', v.tabs.length > 0);
      menu.items = v.items;
    };
    // On a new hash, or new sections moving it (a viewer's without `log`): render, then tell the app.
    const follow = () => {
      const was = r;
      r = route(location.hash, s.sections);
      render();
      if (r.id !== was.id || r.path.join('/') !== was.path.join('/')) s.onNavigate?.(r);
    };
    addEventListener('hashchange', follow);
    menu.mount(slot);
    menu.me.then((me) => (badge.hidden = me?.role !== 'viewer'));
    render();
    return {
      main,
      get route() { return r; },
      update(patch) {
        if (patch && 'sections' in patch) sectioned(patch.sections);
        s = { ...s, ...patch };
        follow();
      },
    };
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

  // Tests reach the pure shell core here.
  globalThis.__misTest?.({ route, layout });
  // Last: everything it uses is defined by now.
  if (typeof document === 'object' && document.addEventListener) onBody(() => menu.start());
})();
