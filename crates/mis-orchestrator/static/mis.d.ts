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
    /** Their latest feedback and what became of it, newest first (owners, editors). */
    feedback?: FeedbackStatus[];
  }

  type FeedbackStatus = { id: string; at: number; note: string } & (
    | { state: 'waiting'; why: string }
    | { state: 'working' }
    | { state: 'draft_ready'; release: number }
    | { state: 'done'; end: 'published'; release: number }
    | { state: 'done'; end: 'rejected' }
    | { state: 'done'; end: 'replaced'; release: number }
    | { state: 'done'; end: 'dismissed'; reply?: string }
    | { state: 'done'; end: 'ended'; status: 'done' | 'failed' | 'cancelled' }
  );

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
    /** Every record `list` finds, following cursors to the end. */
    all<T = unknown>(space: Space, collection: string, q?: Omit<Query, 'cursor'>): Promise<Envelope<T>[]>;
    /** Creates each row (rev 0), 100 to a batch, so a retry or a re-import writes nothing twice:
     * `present` counts ids already there (skipped), `added` the rest (with a retry whose reply was
     * lost: the same data, by you). Each id already there costs a request: leave out ids you
     * know are stored, so it only meets ids added meanwhile. */
    createAll(space: Space, collection: string, rows: { id: string; data: unknown }[]): Promise<{ added: number; present: number }>;
  }

  /** `mis.mock`, only with `?mock=1`. */
  interface MockControls {
    /** Preload records, replacing those ids, whatever the role. */
    seed(records: Seed): Promise<void>;
    /** Act as `role` from now on. */
    setRole(role: Role): Promise<void>;
  }

  /**
   * The platform menu (signed in as, All apps; for owners and editors the app's draft, their
   * feedback and Give feedback). Neutral; theme it with --mis-menu-fg, --mis-menu-bg (its
   * button) and --mis-menu-surface, --mis-menu-text (the menu); --mis-menu-dot (a draft is ready).
   */
  interface Menu {
    /** Put its button in `el` (the app's header); unplaced, it floats top-right. */
    mount(el: Element | null): void;
  }

  /** A screen of the app, at `#/<id>`. `tab`: in the bottom tab bar (when two or more are);
   * else in the ⋯ menu. */
  interface Section {
    id: string;
    label: string;
    tab?: boolean;
  }
  /** `#/log/for/ann` -> `{id: 'log', path: ['log', 'for', 'ann']}`. An empty or unknown hash is
   * the first section, its path kept (`#/p/ann` -> `{id: <first>, path: ['p', 'ann']}`). Parts
   * are as in the hash, percent-encoded. */
  interface Route {
    id: string;
    path: string[];
  }
  interface ShellOptions {
    /** The heading of an app with one section. Default: the page's title. */
    title?: string;
    /** In order; the first is the default. */
    sections: Section[];
    /** In the header instead of the section's label (null: the label). */
    heading?: string | null;
    /** A ‹ link before the heading to this href (null: none). */
    back?: string | null;
    /** Each time the route changes, after the shell has drawn it. */
    onNavigate?(route: Route): void;
  }
  interface Shell {
    /** Render the app's content into this. */
    main: HTMLElement;
    readonly route: Route;
    /** Change options; new sections may move the route (and call onNavigate). */
    update(patch: Partial<ShellOptions>): void;
  }

  interface Sdk {
    MisError: MisErrorClass;
    me(): Promise<Me>;
    records: Records;
    export(): Promise<Export>;
    /** A 409: the record changed (or appeared) since it was read; `current` is what's stored. */
    isConflict(e: unknown): e is MisError & { current: Envelope | null };
    /** What to tell the user when a write fails: a 403 is "You can only view this app.". */
    errorText(e: unknown): string;
    menu: Menu;
    /**
     * The app's frame, appended to the body, once per page: a header (the section's label,
     * View only for viewers, the ⋯ menu), `main` for the content, and a bottom tab bar for `tab`
     * sections (two or more); the other sections go in the menu above its platform items. One
     * section: just the header. Hash routes (`#/<id>`), light and dark, safe areas; the header takes the page's
     * colours and font, the tab bar and menu the --mis-menu-* variables (see Menu) plus
     * --mis-menu-line and --mis-menu-accent (the current tab).
     */
    shell(options: ShellOptions): Shell;
    mock?: MockControls;
  }
}

declare var mis: Mis.Sdk;
