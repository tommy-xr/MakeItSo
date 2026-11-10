// Types for the Make It So apps SDK: `/a/_/mis.js` defines the global `mis`; with `?mock=1` it
// runs on `/a/_/mis-mock.js`, an in-memory twin. Served at `/a/_/mis.d.ts`; `mis apps sdk <dir>`
// writes all three. Add this file to tsconfig `include` (it declares globals, no imports).

declare namespace Mis {
  /** `shared`: every member's. `me`: only its writer's. */
  type Space = 'shared' | 'me';
  /** Viewers read; editors also write; owners also admin. */
  type Role = 'owner' | 'editor' | 'viewer';

  /** A stored record. `rev`: 1 when created, +1 each write. `by`: last writer's email, or
   * `local`. `at`: Unix ms of the last write. `data`: any JSON up to 64 KiB. */
  interface Envelope<T = unknown> {
    id: string;
    rev: number;
    by: string;
    at: number;
    data: T;
  }

  type Scalar = string | number | boolean;
  /** At least one bound; several make a range. Matches only values of the bound's JSON type. */
  interface Condition {
    eq?: Scalar | null;
    gt?: Scalar | null;
    gte?: Scalar | null;
    lt?: Scalar | null;
    lte?: Scalar | null;
  }
  /** Fields are top-level keys of `data` (`[A-Za-z0-9_]{1,64}`). Ties, and no `order_by`, go by id. */
  interface Query {
    where?: Record<string, Condition>;
    order_by?: [field: string, dir: 'asc' | 'desc'];
    /** 1-100; default 100. */
    limit?: number;
    /** From the previous page, with the same `order_by`. */
    cursor?: string;
  }
  interface Page<T = unknown> {
    records: Envelope<T>[];
    /** Pass back for the next page; null: this was the last. */
    cursor: string | null;
  }

  /** A write at the `rev` last read (0: the record must not exist yet). */
  type Op =
    | { op: 'put'; space: Space; collection: string; id: string; rev: number; data: unknown }
    | { op: 'remove'; space: Space; collection: string; id: string; rev: number };

  interface Me {
    user: string;
    role: Role;
    app: string;
    /** The draft release they are trying, on a copy of the records; absent: the live app. */
    draft?: number;
    /** The app's current draft, which owners and editors may try (`/a/<slug>/?draft=<n>`). */
    drafted?: number;
  }

  /** Every record the caller can see, by collection. */
  interface Export {
    version: 1;
    app: string;
    user: string;
    at: number;
    shared: Record<string, Envelope[]>;
    me: Record<string, Envelope[]>;
  }

  /** For `mis.mock.seed`: records by space and collection. `rev`, `by` and `at` default to 1,
   * the mock's user and now, so an `export()` seeds as is. */
  interface Seed {
    shared?: Record<string, SeedRecord[]>;
    me?: Record<string, SeedRecord[]>;
  }
  interface SeedRecord {
    id: string;
    data: unknown;
    rev?: number;
    by?: string;
    at?: number;
  }

  /** How every method rejects. */
  interface MisError extends Error {
    name: 'MisError';
    /** HTTP status: 400, 403, 404, 409, 413… */
    status: number;
    /** On a 409: what is stored now (null: nothing). */
    current?: Envelope | null;
    /** In a batch: the refused op (nothing was written). */
    index?: number;
  }
  interface MisErrorClass {
    new (status: number, message: string, current?: Envelope | null, index?: number): MisError;
  }

  interface Records {
    list<T = unknown>(space: Space, collection: string, q?: Query): Promise<Page<T>>;
    /** null when there is none. */
    get<T = unknown>(space: Space, collection: string, id: string): Promise<Envelope<T> | null>;
    put<T = unknown>(space: Space, collection: string, id: string, rev: number, data: T): Promise<Envelope<T>>;
    /** Removing a missing record succeeds. */
    remove(space: Space, collection: string, id: string, rev: number): Promise<null>;
    /** Up to 100 ops, each id once, in one transaction: all land, or none (rejects with `index`).
     * Each op's record in order (null for a remove). */
    batch(ops: Op[]): Promise<(Envelope | null)[]>;
  }

  /** `mis.mock`, only with `?mock=1`. */
  interface MockControls {
    /** Preload records, replacing those ids, whatever the role. */
    seed(records: Seed): Promise<void>;
    /** Act as `role` from now on. */
    setRole(role: Role): Promise<void>;
  }

  interface Sdk {
    MisError: MisErrorClass;
    me(): Promise<Me>;
    records: Records;
    export(): Promise<Export>;
    mock?: MockControls;
  }
}

declare var mis: Mis.Sdk;
