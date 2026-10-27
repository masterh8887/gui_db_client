'use client';

// Metadata cache per connection: databases -> schemas -> tables, plus per-table details and
// exact counts. Keys are path-like strings so the tree can look things up without nesting.

import type { ColumnInfo, DatabaseInfo, ExactCount, IndexInfo, SchemaInfo, TableInfo, TableRef } from 'ipc-types';
import { create } from 'zustand';
import {
  catalogColumns,
  catalogDatabases,
  catalogDdl,
  catalogIndexes,
  catalogInvalidate,
  catalogLastExactCount,
  catalogSchemas,
  catalogTables,
} from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { tableKey } from '@/lib/utils';

export const dbKey = (connId: string, db: string) => `${connId}/${db}`;
export const schemaKey = (connId: string, db: string, schema: string) => `${connId}/${db}/${schema}`;
export { tableKey };

export interface TableDetails {
  columns?: ColumnInfo[];
  indexes?: IndexInfo[];
  ddl?: string;
}

export interface CatalogState {
  databases: Record<string, DatabaseInfo[]>;
  schemas: Record<string, SchemaInfo[]>;
  tables: Record<string, TableInfo[]>;
  details: Record<string, TableDetails>;
  exactCounts: Record<string, ExactCount>;
  /** Table keys whose exact count job is in flight. */
  counting: Record<string, string>;
  expanded: Record<string, boolean>;
  loading: Record<string, boolean>;
  errors: Record<string, string>;

  toggle(key: string): void;
  setExpanded(key: string, open: boolean): void;
  loadDatabases(connId: string, refresh?: boolean): Promise<DatabaseInfo[]>;
  loadSchemas(connId: string, db: string, refresh?: boolean): Promise<SchemaInfo[]>;
  loadTables(connId: string, db: string, schema: string, refresh?: boolean): Promise<TableInfo[]>;
  loadTableDetails(connId: string, ref: TableRef, refresh?: boolean): Promise<TableDetails>;
  /**
   * Drop everything cached for a connection, here and in the backend (`catalog_invalidate`),
   * and reload the databases. Subtrees loaded afterwards come from the server, not from the
   * backend's metadata cache.
   */
  refresh(connId: string): Promise<void>;
  clearConnection(connId: string): void;
  setCounting(connId: string, ref: TableRef, jobId: string | null): void;
  /** Forget an exact-count job that ended without a count (failed / cancelled). */
  clearCountingJob(jobId: string): void;
  setExactCount(connId: string, ec: ExactCount): void;
  /**
   * Read the table's last finished exact count from the backend (`catalog_last_exact_count`,
   * kept across restarts) and show it, unless the store already holds a newer one (a
   * `job:done` that arrived while the read was in flight). Resolves to the count the store
   * holds afterwards. Never touches the server: only metadata the backend stored locally.
   */
  loadLastExactCount(connId: string, ref: TableRef): Promise<ExactCount | undefined>;
  findTable(connId: string, ref: TableRef): TableInfo | undefined;
}

/**
 * Whether `candidate` should replace `current`: it was counted later. An unparsable timestamp
 * never wins over a count the store already holds (that one came from a `job:done`).
 */
export function isNewerExactCount(candidate: ExactCount, current: ExactCount | undefined): boolean {
  if (!current) return true;
  const a = Date.parse(candidate.at);
  const b = Date.parse(current.at);
  if (Number.isNaN(a)) return false;
  if (Number.isNaN(b)) return true;
  return a > b;
}

function withoutPrefix<T>(rec: Record<string, T>, prefix: string): Record<string, T> {
  const out: Record<string, T> = {};
  for (const [k, v] of Object.entries(rec)) if (!k.startsWith(prefix)) out[k] = v;
  return out;
}

export const useCatalogStore = create<CatalogState>()((set, get) => {
  const setLoading = (key: string, on: boolean) =>
    set((s) => {
      const loading = { ...s.loading };
      if (on) loading[key] = true;
      else delete loading[key];
      return { loading };
    });
  const setError = (key: string, msg: string | null) =>
    set((s) => {
      const errors = { ...s.errors };
      if (msg) errors[key] = msg;
      else delete errors[key];
      return { errors };
    });

  /** Loads in flight per key: a second request for the same node joins the first one. */
  const inflight = new Map<string, Promise<unknown>>();

  function run<T>(key: string, fn: () => Promise<T>): Promise<T> {
    const pending = inflight.get(key);
    if (pending) return pending as Promise<T>;
    const p = (async () => {
      setLoading(key, true);
      setError(key, null);
      try {
        return await fn();
      } catch (e) {
        setError(key, errorText(e));
        throw e;
      } finally {
        setLoading(key, false);
        inflight.delete(key);
      }
    })();
    inflight.set(key, p);
    return p;
  }

  return {
    databases: {},
    schemas: {},
    tables: {},
    details: {},
    exactCounts: {},
    counting: {},
    expanded: {},
    loading: {},
    errors: {},

    toggle: (key) => set((s) => ({ expanded: { ...s.expanded, [key]: !s.expanded[key] } })),
    setExpanded: (key, open) => set((s) => ({ expanded: { ...s.expanded, [key]: open } })),

    loadDatabases: async (connId, refresh = false) => {
      const cached = get().databases[connId];
      if (cached && !refresh) return cached;
      return run(connId, async () => {
        const list = await catalogDatabases(connId, refresh);
        set((s) => ({ databases: { ...s.databases, [connId]: list } }));
        return list;
      });
    },

    loadSchemas: async (connId, db, refresh = false) => {
      const key = dbKey(connId, db);
      const cached = get().schemas[key];
      if (cached && !refresh) return cached;
      return run(key, async () => {
        const list = await catalogSchemas(connId, db, refresh);
        set((s) => ({ schemas: { ...s.schemas, [key]: list } }));
        return list;
      });
    },

    loadTables: async (connId, db, schema, refresh = false) => {
      const key = schemaKey(connId, db, schema);
      const cached = get().tables[key];
      if (cached && !refresh) return cached;
      return run(key, async () => {
        const list = await catalogTables(connId, db, schema, refresh);
        set((s) => ({ tables: { ...s.tables, [key]: list } }));
        return list;
      });
    },

    loadTableDetails: async (connId, ref, refresh = false) => {
      const key = tableKey(connId, ref);
      const cached = get().details[key];
      if (cached?.columns && cached.indexes && cached.ddl !== undefined && !refresh) return cached;
      return run(key, async () => {
        const [columns, indexes, ddl] = await Promise.all([
          catalogColumns(connId, ref),
          catalogIndexes(connId, ref),
          catalogDdl(connId, ref),
        ]);
        const details: TableDetails = { columns, indexes, ddl };
        set((s) => ({ details: { ...s.details, [key]: details } }));
        return details;
      });
    },

    refresh: async (connId) => {
      // `refresh = true` below only bypasses the backend cache for the databases list; the
      // schemas / tables / details that expanded nodes reload next would still come from it.
      try {
        await catalogInvalidate(connId);
      } catch (e) {
        setError(connId, errorText(e));
        throw e;
      }
      get().clearConnection(connId);
      await get().loadDatabases(connId, true);
    },

    clearConnection: (connId) =>
      set((s) => {
        const prefix = `${connId}/`;
        const databases = { ...s.databases };
        delete databases[connId];
        return {
          databases,
          schemas: withoutPrefix(s.schemas, prefix),
          tables: withoutPrefix(s.tables, prefix),
          details: withoutPrefix(s.details, prefix),
          errors: withoutPrefix(s.errors, prefix),
        };
      }),

    setCounting: (connId, ref, jobId) =>
      set((s) => {
        const key = tableKey(connId, ref);
        const counting = { ...s.counting };
        if (jobId) counting[key] = jobId;
        else delete counting[key];
        return { counting };
      }),

    clearCountingJob: (jobId) =>
      set((s) => {
        const entries = Object.entries(s.counting);
        if (!entries.some(([, id]) => id === jobId)) return s;
        return { counting: Object.fromEntries(entries.filter(([, id]) => id !== jobId)) };
      }),

    setExactCount: (connId, ec) =>
      set((s) => {
        const key = tableKey(connId, ec.table);
        const counting = { ...s.counting };
        delete counting[key];
        return { exactCounts: { ...s.exactCounts, [key]: ec }, counting };
      }),

    loadLastExactCount: async (connId, ref) => {
      const key = tableKey(connId, ref);
      const stored = await catalogLastExactCount(connId, ref);
      if (stored) {
        set((s) => (isNewerExactCount(stored, s.exactCounts[key]) ? { exactCounts: { ...s.exactCounts, [key]: stored } } : s));
      }
      return get().exactCounts[key];
    },

    findTable: (connId, ref) => {
      const s = get();
      for (const [key, list] of Object.entries(s.tables)) {
        if (!key.startsWith(`${connId}/`)) continue;
        if (ref.database && !key.startsWith(`${connId}/${ref.database}/`)) continue;
        const hit = list.find((t) => t.name === ref.name && (!ref.schema || t.schema === ref.schema));
        if (hit) return hit;
      }
      return undefined;
    },
  };
});
