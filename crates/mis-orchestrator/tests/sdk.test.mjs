// The apps SDK (static/mis.js) against a stubbed fetch, its in-memory twin
// (static/mis-mock.mjs), and their types (static/mis.d.ts).
// Run: node --test crates/mis-orchestrator/tests/sdk.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createMock, MAX_BATCH, MAX_DATA } from '../static/mis-mock.mjs';

const file = (name) => readFileSync(new URL(`../static/${name}`, import.meta.url), 'utf8');
const src = file('mis.js');

// mis.js loaded on /a/demo/<path> from `script`, its fetches answered by `reply(url, init)`; the calls seen.
function load(reply, search = '', script = 'http://x/a/_/mis.js') {
  const calls = [];
  const g = {
    location: { pathname: '/a/demo/list', search },
    document: { currentScript: { src: script } },
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

test('mis.js turns refusals into MisErrors and asks for a missing record as null', async () => {
  const { mis, calls } = load((url, init) =>
    !init ? json(200, null)
      : JSON.parse(init.body).rev === 0 ? json(409, { current: env })
      : new Response("a viewer can't do that here", { status: 403 }));
  assert.equal(await mis.records.get('shared', 'notes', 'zz'), null);
  assert.equal(calls[0].url, '/a/demo/api/v1/records/shared/notes/zz?missing=null');
  await assert.rejects(mis.records.put('shared', 'notes', 'a', 0, {}), (e) => e instanceof mis.MisError && e.status === 409 && e.current.rev === 1 && e.index === undefined);
  await assert.rejects(mis.records.put('shared', 'notes', 'a', 1, {}), (e) => e.status === 403 && /viewer/.test(e.message));
  // Not live: still an error, not a quiet null.
  const gone = load(() => new Response('', { status: 404, statusText: 'Not Found' }));
  await assert.rejects(gone.mis.records.get('shared', 'notes', 'zz'), (e) => e.status === 404);
});

test('mis.js batches POST {ops} and name the refused op', async () => {
  const ops = [
    { op: 'put', space: 'shared', collection: 'notes', id: 'a', rev: 0, data: { t: 1 } },
    { op: 'remove', space: 'shared', collection: 'notes', id: 'b', rev: 2 },
  ];
  let answer = json(200, { records: [env, null] });
  const { mis, calls } = load(() => answer);
  assert.deepEqual(await mis.records.batch(ops), [env, null]);
  assert.equal(calls[0].url, '/a/demo/api/v1/mutations');
  assert.deepEqual(JSON.parse(calls[0].init.body), { ops });
  answer = json(409, { current: env, index: 1 });
  await assert.rejects(mis.records.batch(ops), (e) => e.status === 409 && e.index === 1 && e.current.rev === 1);
  answer = new Response('ops[1]: data is 70000 bytes; the most is 65536', { status: 413 });
  await assert.rejects(mis.records.batch(ops), (e) => e.status === 413 && e.index === 1);
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

test('mock: a batch lands whole or not at all', async () => {
  const m = createMock();
  const p = (id, rev, data) => ({ op: 'put', space: 'shared', collection: 'notes', id, rev, data });
  await m.records.put('shared', 'notes', 'a', 0, { n: 1 });
  // A conflict at ops[1]: ops[0] isn't kept.
  await assert.rejects(m.records.batch([p('b', 0, 1), p('a', 0, 9)]), (e) => e.status === 409 && e.index === 1 && e.current.rev === 1);
  assert.equal(await m.records.get('shared', 'notes', 'b'), null);
  const ops = [p('b', 0, 1), p('a', 1, 9), { op: 'remove', space: 'shared', collection: 'notes', id: 'zz', rev: 3 }];
  const done = await m.records.batch(ops);
  assert.deepEqual(done.map((e) => e && e.rev), [1, 2, null]);
  // Sent again after a lost answer: the same records.
  assert.deepEqual(await m.records.batch(ops), done);
  await assert.rejects(m.records.batch([p('c', 0, 1), p('big', 0, 'x'.repeat(MAX_DATA))]), (e) => e.status === 413 && e.index === 1 && /^ops\[1\]: /.test(e.message));
  await assert.rejects(m.records.batch([p('d', 0, 1), p('d', 1, 2)]), (e) => e.status === 400 && e.index === 1 && e.message === 'ops[1]: shared/notes/d is already in this batch');
  // Every op's shape is checked before any applies (a stale ops[0] doesn't hide a malformed ops[1]).
  await assert.rejects(m.records.batch([p('a', 0, 1), { op: 'put', space: 'shared', collection: 'notes', id: 'e', rev: 0 }]), (e) => e.status === 400 && e.index === 1 && /^ops\[1\]: bad mutation/.test(e.message));
  await assert.rejects(m.records.batch([{ op: 'put', space: 'shared', collection: 'notes', rev: 0, data: 1 }]), (e) => e.status === 400 && e.index === 0);
  await assert.rejects(m.records.put('shared', 'notes', undefined, 0, 1), (e) => e.status === 400);
  const many = Array.from({ length: MAX_BATCH + 1 }, (_, i) => p(`m${i}`, 0, 1));
  await assert.rejects(m.records.batch(many), (e) => e.status === 400);
  assert.equal((await m.records.batch(many.slice(1))).length, MAX_BATCH);
  for (const id of ['c', 'd', 'm0']) assert.equal(await m.records.get('shared', 'notes', id), null, id);
  await m.setRole('viewer');
  await assert.rejects(m.records.batch([p('v', 0, 1)]), (e) => e.status === 403);
  assert.equal(await m.records.get('shared', 'notes', 'v'), null);
});

test('mock: seed preloads records, whatever the role; an export seeds as is', async () => {
  const m = createMock({ role: 'viewer', user: 'me@example.com' });
  await m.seed({ shared: { notes: [{ id: 'a', data: { t: 1 } }, { id: 'b', rev: 4, by: 'x@example.com', at: 7, data: 2 }] }, me: { drafts: [{ id: 'd', data: 'mine' }] } });
  const a = await m.records.get('shared', 'notes', 'a');
  assert.deepEqual([a.rev, a.by, a.data], [1, 'me@example.com', { t: 1 }]);
  assert.deepEqual(await m.records.get('shared', 'notes', 'b'), { id: 'b', rev: 4, by: 'x@example.com', at: 7, data: 2 });
  assert.equal((await m.records.get('me', 'drafts', 'd')).data, 'mine');
  // Seeded revisions behave like stored ones.
  await m.setRole('editor');
  await assert.rejects(m.records.put('shared', 'notes', 'b', 3, 0), (e) => e.status === 409 && e.current.rev === 4);
  assert.equal((await m.records.put('shared', 'notes', 'b', 4, 0)).rev, 5);
  const twin = createMock({ user: 'me@example.com' });
  await twin.seed(await m.export());
  assert.deepEqual((await twin.export()).shared, (await m.export()).shared);
  // All or nothing.
  for (const bad of [{ shared: { notes: [{ id: 'ok', data: 1 }, { id: 'a/b', data: 1 }] } }, { shared: { Notes: [{ id: 'a', data: 1 }] } }, { shared: { notes: [{ id: 'x' }] } }, { shared: { notes: [{ data: 1 }] } }, { shared: { notes: [{ id: 'x', data: 1, rev: 0 }] } }, { shared: { notes: [{ id: 'x', data: 'x'.repeat(MAX_DATA) }] } }]) {
    await assert.rejects(twin.seed(bad), (e) => e.status >= 400, JSON.stringify(bad).slice(0, 80));
  }
  assert.equal(await twin.records.get('shared', 'notes', 'ok'), null);
});

test('mis.js in mock mode loads mis-mock.js beside it, with no server', async () => {
  // As `mis apps sdk <dir>` writes them, for an app's dev server to serve at /a/_/.
  const dir = mkdtempSync(join(tmpdir(), 'mis-sdk-'));
  writeFileSync(join(dir, 'mis-mock.js'), file('mis-mock.mjs'));
  writeFileSync(join(dir, 'package.json'), '{"type": "module"}');
  const { mis, calls } = load(() => assert.fail('no fetch in mock mode'), '?mock=1&role=viewer', pathToFileURL(join(dir, 'mis.js')).href);
  assert.equal((await mis.me()).role, 'viewer');
  await mis.mock.seed({ shared: { notes: [{ id: 'a', data: 1 }] } });
  assert.equal((await mis.records.get('shared', 'notes', 'a')).data, 1);
  await assert.rejects(mis.records.batch([{ op: 'remove', space: 'shared', collection: 'notes', id: 'a', rev: 1 }]), (e) => e instanceof mis.MisError && e.status === 403);
  await mis.mock.setRole('editor');
  assert.deepEqual(await mis.records.batch([{ op: 'remove', space: 'shared', collection: 'notes', id: 'a', rev: 1 }]), [null]);
  assert.equal(calls.length, 0);
});

// The members of `interface name` in mis.d.ts.
function members(name) {
  const dts = file('mis.d.ts');
  const body = new RegExp(`interface ${name}\\b[^{]*\\{\\n([\\s\\S]*?)\\n  \\}`).exec(dts);
  assert.ok(body, name);
  return [...body[1].matchAll(/^    (\w+)\??[(<:]/gm)].map((m) => m[1]).sort();
}
const keys = (o) => Object.keys(o).sort();

test('mis.d.ts declares exactly what mis.js and the mock define', async () => {
  const { mis } = load(() => json(200, null));
  assert.deepEqual(keys(mis), members('Sdk').filter((k) => k !== 'mock'));
  assert.deepEqual(keys(mis.records), members('Records'));
  const mocked = load(() => json(200, null), '?mock=1', 'file:///nowhere/mis.js').mis;
  assert.deepEqual(keys(mocked), members('Sdk'));
  assert.deepEqual(keys(mocked.mock), members('MockControls'));
  await assert.rejects(mocked.me()); // no mis-mock.js at /nowhere
  const m = createMock();
  const top = members('Sdk').filter((k) => !['MisError', 'mock'].includes(k));
  assert.deepEqual(keys(m), [...top, ...members('MockControls')].sort());
  assert.deepEqual(keys(m.records), members('Records'));
  const err = new mis.MisError(409, 'conflict', null, 2);
  assert.deepEqual([err.name, err.status, err.current, err.index], ['MisError', 409, null, 2]);
  assert.deepEqual(members('MisError'), ['current', 'index', 'name', 'status']);
});
