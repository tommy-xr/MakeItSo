// The apps SDK (static/mis.js) against a stubbed fetch, and its in-memory twin
// (static/mis-mock.mjs). Run: node --test crates/mis-orchestrator/tests/sdk.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createMock, MAX_DATA } from '../static/mis-mock.mjs';

const src = readFileSync(new URL('../static/mis.js', import.meta.url), 'utf8');

// mis.js loaded on /a/demo/<path>, its fetches answered by `reply(url, init)`; the calls seen.
function load(reply, search = '') {
  const calls = [];
  const g = {
    location: { pathname: '/a/demo/list', search },
    document: { currentScript: { src: 'http://x/a/_/mis.js' } },
    fetch: async (url, init) => {
      calls.push({ url, init });
      return reply(url, init);
    },
  };
  new Function('globalThis', 'location', 'document', 'fetch', src)(g, g.location, g.document, g.fetch);
  return { mis: g.mis, calls };
}

const json = (status, body) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const env = { id: 'a', rev: 1, by: 'me@example.com', at: 1, data: { t: 1 } };

test('mis.js speaks the HTTP API under /a/<slug>/api/v1/', async () => {
  const { mis, calls } = load((url) =>
    url.endsWith('/me') ? json(200, { user: 'me@example.com', role: 'editor', app: 'demo' })
      : url.includes('/mutations') ? json(200, { record: env })
      : json(200, { records: [env], cursor: null }));
  assert.equal((await mis.me()).role, 'editor');
  assert.deepEqual(await mis.records.put('shared', 'notes', 'a', 0, { t: 1 }), env);
  const q = { where: { done: { eq: false } }, order_by: ['at', 'desc'], limit: 5 };
  assert.deepEqual(await mis.records.list('me', 'notes', q), { records: [env], cursor: null });
  assert.equal(await mis.records.remove('shared', 'notes', 'a', 1).then(() => 'ok'), 'ok');
  const [me, put, list, remove] = calls;
  assert.equal(me.url, '/a/demo/api/v1/me');
  assert.equal(put.url, '/a/demo/api/v1/mutations');
  assert.equal(put.init.method, 'POST');
  assert.equal(put.init.headers['Content-Type'], 'application/json');
  assert.deepEqual(JSON.parse(put.init.body), { op: 'put', space: 'shared', collection: 'notes', id: 'a', rev: 0, data: { t: 1 } });
  assert.equal(list.url, `/a/demo/api/v1/records/me/notes?q=${encodeURIComponent(JSON.stringify(q))}`);
  assert.deepEqual(JSON.parse(remove.init.body), { op: 'remove', space: 'shared', collection: 'notes', id: 'a', rev: 1 });
});

test('mis.js turns refusals into MisErrors and a missing record into null', async () => {
  const { mis } = load((url, init) =>
    !init ? new Response('no such record', { status: 404 })
      : JSON.parse(init.body).rev === 0 ? json(409, { current: env })
      : new Response("a viewer can't do that here", { status: 403 }));
  assert.equal(await mis.records.get('shared', 'notes', 'zz'), null);
  await assert.rejects(mis.records.put('shared', 'notes', 'a', 0, {}), (e) => e.name === 'MisError' && e.status === 409 && e.current.rev === 1);
  await assert.rejects(mis.records.put('shared', 'notes', 'a', 1, {}), (e) => e.status === 403 && /viewer/.test(e.message));
});

test('mock: create, update, stale and create-only conflicts', async () => {
  const m = createMock({ user: 'me@example.com' });
  const a = await m.records.put('shared', 'notes', 'a', 0, { t: 1 });
  assert.deepEqual([a.rev, a.by], [1, 'me@example.com']);
  await assert.rejects(m.records.put('shared', 'notes', 'a', 0, { t: 2 }), (e) => e.status === 409 && e.current.rev === 1);
  const b = await m.records.put('shared', 'notes', 'a', 1, { t: 2 });
  assert.equal(b.rev, 2);
  await assert.rejects(m.records.put('shared', 'notes', 'a', 3, { t: 3 }), (e) => e.status === 409 && e.current.rev === 2);
  await assert.rejects(m.records.put('shared', 'notes', 'nope', 4, {}), (e) => e.status === 409 && e.current === null);
});

test('mock: an identical retry succeeds; removes of missing records too', async () => {
  const m = createMock();
  const a = await m.records.put('shared', 'notes', 'a', 0, { x: 1, y: [1] });
  // The same create again, keys in another order: it already landed.
  assert.deepEqual(await m.records.put('shared', 'notes', 'a', 0, { y: [1], x: 1 }), a);
  // Same rev, other data: a conflict.
  await assert.rejects(m.records.put('shared', 'notes', 'a', 0, { x: 2 }), (e) => e.status === 409);
  await assert.rejects(m.records.remove('shared', 'notes', 'a', 9), (e) => e.status === 409);
  assert.equal(await m.records.remove('shared', 'notes', 'a', 1), null);
  assert.equal(await m.records.remove('shared', 'notes', 'a', 1), null);
  assert.equal(await m.records.get('shared', 'notes', 'a'), null);
});

test('mock: viewers read but get 403 on writes; role settable', async () => {
  const m = createMock({ role: 'viewer' });
  assert.equal((await m.me()).role, 'viewer');
  await assert.rejects(m.records.put('shared', 'notes', 'a', 0, {}), (e) => e.status === 403);
  await assert.rejects(m.records.remove('shared', 'notes', 'a', 0), (e) => e.status === 403);
  assert.deepEqual(await m.records.list('shared', 'notes'), { records: [], cursor: null });
  m.setRole('editor');
  assert.equal((await m.records.put('shared', 'notes', 'a', 0, {})).rev, 1);
});

test('mock: caps and names are checked like the server', async () => {
  const m = createMock();
  await assert.rejects(m.records.put('shared', 'notes', 'a', 0, 'x'.repeat(MAX_DATA)), (e) => e.status === 413);
  for (const [c, id] of [['Notes', 'a'], ['notes', 'a/b'], ['notes', '..'], ['notes', '']]) {
    await assert.rejects(m.records.put('shared', c, id, 0, {}), (e) => e.status === 400, `${c} ${id}`);
  }
  await assert.rejects(m.records.list('shared', 'notes', { limit: 101 }), (e) => e.status === 400);
  await assert.rejects(m.records.list('shared', 'notes', { where: { 'a.b': { eq: 1 } } }), (e) => e.status === 400);
  await assert.rejects(m.records.list('shared', 'notes', { where: { a: {} } }), (e) => e.status === 400);
});

test('mock: queries filter by type, order with ties by id, and page with cursors', async () => {
  const m = createMock();
  const rows = { a: { score: 3, done: true }, b: { score: 1 }, c: { score: 3 }, d: { other: 1 }, e: { score: '3' }, f: { score: 2.5, done: false }, g: { score: true } };
  for (const [id, data] of Object.entries(rows)) await m.records.put('shared', 'notes', id, 0, data);
  const ids = async (q) => (await m.records.list('shared', 'notes', q)).records.map((r) => r.id);
  assert.deepEqual(await ids({ where: { score: { eq: 3 } } }), ['a', 'c']);
  assert.deepEqual(await ids({ where: { score: { eq: '3' } } }), ['e']);
  assert.deepEqual(await ids({ where: { score: { gte: 2, lt: 3 } } }), ['f']);
  assert.deepEqual(await ids({ where: { done: { eq: false } } }), ['f']);
  // The same order as the server's (see records_db tests).
  assert.deepEqual(await ids({ order_by: ['score', 'asc'] }), ['d', 'b', 'g', 'f', 'a', 'c', 'e']);
  for (const dir of ['asc', 'desc']) {
    const whole = await ids({ order_by: ['score', dir] });
    const seen = [];
    let cursor;
    do {
      const p = await m.records.list('shared', 'notes', { order_by: ['score', dir], limit: 2, cursor });
      seen.push(...p.records.map((r) => r.id));
      cursor = p.cursor;
    } while (cursor);
    assert.deepEqual(seen, whole, dir);
  }
  const p = await m.records.list('shared', 'notes', { order_by: ['score', 'asc'], limit: 1 });
  await assert.rejects(m.records.list('shared', 'notes', { cursor: p.cursor }), (e) => e.status === 400);
});

test('mock: me records are their writer\'s, and export shows what the caller sees', async () => {
  const m = createMock({ user: 'me@example.com', app: 'demo' });
  await m.records.put('shared', 'notes', 'n', 0, { t: 'all' });
  await m.records.put('me', 'drafts', 'd', 0, { t: 'mine' });
  const x = await m.export();
  assert.deepEqual([x.version, x.app, x.user], [1, 'demo', 'me@example.com']);
  assert.deepEqual(Object.keys(x.shared), ['notes']);
  assert.equal(x.me.drafts[0].data.t, 'mine');
  assert.equal((await m.records.get('shared', 'drafts', 'd')), null);
});

test('mock: a recreated id continues its revisions, so a late remove conflicts', async () => {
  const m = createMock();
  await m.records.put('shared', 'notes', 'a', 0, { v: 1 });
  await m.records.remove('shared', 'notes', 'a', 1);
  const again = await m.records.put('shared', 'notes', 'a', 0, { v: 2 });
  assert.equal(again.rev, 2);
  await assert.rejects(m.records.remove('shared', 'notes', 'a', 1), (e) => e.status === 409 && e.current.rev === 2);
  // Its own create, retried: still a success.
  assert.deepEqual(await m.records.put('shared', 'notes', 'a', 0, { v: 2 }), again);
});

test('mock: refuses what the server refuses, and any collection name works', async () => {
  const m = createMock();
  for (const q of [{ order_by: ['score'] }, { order_by: ['score', 'asc', 'x'] }, { order_by: {} }, { order_by: false }, { order_by: [123, 'asc'] }, { filter: {} }, { '': 1 }, { where: { a: { constructor: 1 } } }, { where: { a: { eq: 1, typo: null } } }, { where: null }, { order_by: '' }]) {
    await assert.rejects(m.records.list('shared', 'notes', q), (e) => e.status === 400, JSON.stringify(q));
  }
  for (const c of ['constructor', '__proto__']) await m.records.put('shared', c, 'a', 0, { constructor: 1 });
  const x = await m.export();
  assert.deepEqual(Object.keys(x.shared).sort(), ['__proto__', 'constructor']);
  assert.deepEqual(await m.records.list('shared', 'constructor', { where: { toString: { eq: 1 } } }), { records: [], cursor: null });
});

test('mock: rejects with the error class it is given (mis.js passes mis.MisError)', async () => {
  class E extends Error {}
  const m = createMock({ role: 'viewer', Err: E });
  await assert.rejects(m.records.put('shared', 'notes', 'a', 0, {}), (e) => e instanceof E);
});
