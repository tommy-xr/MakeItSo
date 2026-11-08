// In-memory twin of the apps records API (see mis.js), for screenshots and tests without a
// server: same envelopes, revisions, conflicts (409), create-only, identical retries, caps,
// queries and role checks (viewers get 403 on writes). Nothing persists across reloads.
export const MAX_DATA = 64 << 10;
export const MAX_PAGE = 100;

export class MisError extends Error {
  constructor(status, message, current) {
    super(message);
    this.name = 'MisError';
    this.status = status;
    if (current !== undefined) this.current = current;
  }
}

const validId = (s) => typeof s === 'string' && /^[A-Za-z0-9._:-]{1,128}$/.test(s) && s !== '.' && s !== '..';
const validCollection = (s) => typeof s === 'string' && /^[a-z0-9_-]{1,64}$/.test(s);
const validField = (s) => typeof s === 'string' && /^[A-Za-z0-9_]{1,64}$/.test(s);
const SPACES = ['shared', 'me'];

// JSON with sorted keys, so equal data compares equal however it was built.
const canon = (v) =>
  v && typeof v === 'object' && !Array.isArray(v)
    ? `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canon(v[k])}`).join(',')}}`
    : Array.isArray(v) ? `[${v.map(canon).join(',')}]` : JSON.stringify(v);
const copy = (v) => (v === undefined ? v : JSON.parse(JSON.stringify(v)));

// A top-level field of `data`; undefined when it has none.
const field = (data, f) => (data && typeof data === 'object' && !Array.isArray(data) && Object.hasOwn(data, f) ? data[f] : undefined);
// A value as SQLite orders it: missing/null < numbers (booleans as 1/0) < text (objects and
// arrays as their JSON). [rank, value].
function key(v) {
  if (v === undefined || v === null) return [0, null];
  if (typeof v === 'boolean') return [1, v ? 1 : 0];
  if (typeof v === 'number') return [1, v];
  return [2, typeof v === 'string' ? v : JSON.stringify(v)];
}
// Text compares by UTF-16 units, SQLite by UTF-8 bytes: they differ only past U+E000.
const cmp = (a, b) => (a[0] - b[0]) || (a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0);
const jsonType = (v) =>
  v === null || v === undefined ? 'null' : typeof v === 'boolean' ? 'bool' : typeof v === 'number' ? 'number' : typeof v === 'string' ? 'text' : 'other';

// `Err`: the error class to reject with (mis.js passes its own, so `instanceof mis.MisError` holds).
export function createMock({ role = 'owner', user = 'mock@example.com', app = 'mock', Err = MisError } = {}) {
  // `${space}\0${owner}\0${collection}\0${id}` -> {e: envelope, base}, base being the
  // revisions the id reached before e was created; `gone`: the same keys -> a removed
  // record's last rev, which a new record continues from (so late writes conflict).
  const rows = new Map();
  const gone = new Map();
  const owner = (space) => (space === 'me' ? user : '');
  const rowKey = (space, collection, id) => [space, owner(space), collection, id].join('\0');
  const fail = (status, message, current) => Promise.reject(new Err(status, message, current));
  const check = (space, collection, id) => {
    if (!SPACES.includes(space)) return 'bad space';
    if (!validCollection(collection)) return `collection ${JSON.stringify(collection)}: expected [a-z0-9_-]{1,64}`;
    if (id !== undefined && !validId(id)) return `id ${JSON.stringify(id)}: expected 1-128 of [A-Za-z0-9._:-]`;
    return null;
  };

  function mutate(op) {
    if (role === 'viewer') return fail(403, `a viewer can't do that here`);
    const bad = check(op.space, op.collection, op.id);
    if (bad) return fail(400, bad);
    if (!Number.isInteger(op.rev) || op.rev < 0) return fail(400, 'rev must be a whole number');
    const k = rowKey(op.space, op.collection, op.id);
    const { e: cur, base = 0 } = rows.get(k) || {};
    if (op.op === 'remove') {
      if (!cur) return Promise.resolve(null);
      if (cur.rev !== op.rev) return fail(409, 'conflict', copy(cur));
      rows.delete(k);
      gone.set(k, cur.rev);
      return Promise.resolve(null);
    }
    if (op.data === undefined) return fail(400, 'bad mutation: missing field `data`');
    const len = new TextEncoder().encode(JSON.stringify(op.data)).length;
    if (len > MAX_DATA) return fail(413, `data is ${len} bytes; the most is ${MAX_DATA}`);
    const put = (rev, base) => {
      const e = { id: op.id, rev, by: user, at: Date.now(), data: copy(op.data) };
      rows.set(k, { e, base });
      gone.delete(k);
      return Promise.resolve(copy(e));
    };
    if (!cur) {
      const last = gone.get(k) || 0;
      return op.rev === 0 ? put(last + 1, last) : fail(409, 'conflict', null);
    }
    if (cur.rev === op.rev) return put(op.rev + 1, base);
    // The response to this very write was lost: it landed, so it succeeded.
    const landed = op.rev === 0 ? cur.rev === base + 1 : op.rev > base && cur.rev === op.rev + 1;
    if (landed && cur.by === user && canon(cur.data) === canon(op.data)) return Promise.resolve(copy(cur));
    return fail(409, 'conflict', copy(cur));
  }

  function list(space, collection, q = {}) {
    const bad = check(space, collection);
    if (bad) return fail(400, bad);
    const limit = q.limit ?? MAX_PAGE;
    if (!Number.isInteger(limit) || limit < 1 || limit > MAX_PAGE) return fail(400, `limit must be 1-${MAX_PAGE}`);
    const unknown = Object.keys(q).find((k) => !['where', 'order_by', 'limit', 'cursor'].includes(k));
    if (unknown !== undefined) return fail(400, `q: unknown field \`${unknown}\``);
    const o = q.order_by ?? null;
    const ordered = o === null || (Array.isArray(o) && o.length === 2 && validField(o[0]) && ['asc', 'desc'].includes(o[1]));
    if (!ordered) return fail(400, 'order_by: expected [field, "asc" | "desc"]');
    const [by, dir] = o || [];
    if (q.where !== undefined && (q.where === null || typeof q.where !== 'object' || Array.isArray(q.where))) return fail(400, 'where: expected an object');
    const conds = Object.entries(q.where || {});
    const ops = { eq: (c) => c === 0, gt: (c) => c > 0, gte: (c) => c >= 0, lt: (c) => c < 0, lte: (c) => c <= 0 };
    for (const [f, cond] of conds) {
      if (!validField(f)) return fail(400, `field ${JSON.stringify(f)}: expected [A-Za-z0-9_]{1,64}`);
      const all = Object.entries(cond && typeof cond === 'object' ? cond : {});
      const strange = all.find(([op]) => !Object.hasOwn(ops, op));
      if (strange) return fail(400, `where.${f}: unknown ${strange[0]}`);
      const given = all.filter(([, v]) => v !== null);
      if (!given.length) return fail(400, `where.${f}: give eq, gt, gte, lt or lte`);
      for (const [, v] of given) {
        if (!['bool', 'number', 'text'].includes(jsonType(v))) return fail(400, `where.${f}: values are strings, numbers or booleans`);
      }
    }
    const match = (e) =>
      conds.every(([f, cond]) =>
        Object.entries(cond).filter(([, v]) => v !== null).every(([op, v]) => {
          const have = field(e.data, f);
          return jsonType(have) === jsonType(v) && ops[op](cmp(key(have), key(v)));
        }));
    // A row's place: its order value, then its id.
    const place = (e) => ({ k: by ? key(field(e.data, by)) : [0, null], id: e.id });
    const sign = dir === 'desc' ? -1 : 1;
    const order = (a, b) => sign * (cmp(a.k, b.k) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    const prefix = [space, owner(space), collection, ''].join('\0');
    let found = [...rows.entries()]
      .filter(([k, { e }]) => k.startsWith(prefix) && match(e))
      .map(([, { e }]) => e)
      .sort((a, b) => order(place(a), place(b)));
    if (q.cursor) {
      let after;
      try {
        after = JSON.parse(q.cursor);
      } catch {
        return fail(400, 'bad cursor');
      }
      if (canon(after.order) !== canon(q.order_by || null)) return fail(400, 'cursor is from a query with another order_by');
      found = found.filter((e) => order(place(e), after) > 0);
    }
    const page = found.slice(0, limit);
    const last = page[page.length - 1];
    const cursor = found.length > limit ? JSON.stringify({ order: q.order_by || null, ...place(last) }) : null;
    return Promise.resolve({ records: copy(page), cursor });
  }

  function grouped(space) {
    const out = Object.create(null); // a collection may be named `constructor`
    const all = [...rows.entries()].filter(([k]) => k.startsWith(`${space}\0${owner(space)}\0`));
    for (const [k, { e }] of all.sort(([a], [b]) => (a < b ? -1 : 1))) (out[k.split('\0')[2]] ||= []).push(copy(e));
    return out;
  }

  return {
    me: () => Promise.resolve({ user, role, app }),
    records: {
      list,
      get: (space, collection, id) => {
        const bad = check(space, collection, id);
        if (bad) return fail(400, bad);
        return Promise.resolve(copy(rows.get(rowKey(space, collection, id))?.e) ?? null);
      },
      put: (space, collection, id, rev, data) => mutate({ op: 'put', space, collection, id, rev, data }),
      remove: (space, collection, id, rev) => mutate({ op: 'remove', space, collection, id, rev }),
    },
    export: () =>
      Promise.resolve({ version: 1, app, user, at: Date.now(), shared: grouped('shared'), me: grouped('me') }),
    /** For tests: act as another role from now on. */
    setRole: (r) => {
      role = r;
    },
  };
}
