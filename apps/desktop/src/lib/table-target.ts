// Where queries run and whether a browsed table can be queried there (DESIGN.md §7.5). Pure
// reads of the connections and catalog stores, so the tabs store can use them; re-exported by
// `lib/table-actions`, which is where callers import them from.

import type { TableRef } from 'ipc-types';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { defineStrings } from './i18n';

const ts = defineStrings('tableTarget', {
  en: {
    browseOnly: 'This database is browse-only. Queries run on {db}.',
    noTable: 'Table not found',
  },
  ko: {
    browseOnly: '탐색 전용 데이터베이스입니다. 쿼리는 {db}에서 실행됩니다',
    noTable: '테이블을 찾을 수 없습니다',
  },
});

/** Drivers whose select-all SQL qualifies the database too (`db.schema.table`). */
const DB_QUALIFYING = new Set(['mysql', 'mariadb', 'mssql']);

/**
 * The database queries of this connection run in: `conn.database`, else the probed
 * `ServerInfo.database`, else the catalog's `isCurrent` database; `null` when none is known.
 */
export function execDatabase(connId: string): string | null {
  const cs = useConnectionsStore.getState();
  const conn = cs.connections.find((c) => c.id === connId);
  if (conn?.database) return conn.database;
  const probed = cs.lastProbe[connId]?.server.database;
  if (probed) return probed;
  return useCatalogStore.getState().databases[connId]?.find((d) => d.isCurrent)?.name ?? null;
}

/** Reactive `execDatabase` for components (pill, sidebar `(실행)` marks, the browse-only row). */
export function useExecDatabase(connId: string | null): string | null {
  const fromConn = useConnectionsStore((s) => (connId ? s.connections.find((c) => c.id === connId)?.database : undefined));
  const fromProbe = useConnectionsStore((s) => (connId ? s.lastProbe[connId]?.server.database : undefined));
  const fromCatalog = useCatalogStore((s) => (connId ? s.databases[connId]?.find((d) => d.isCurrent)?.name : undefined));
  return fromConn || fromProbe || fromCatalog || null;
}

/**
 * Whether `ref` can be the target of an extraction query on `connId`. PostgreSQL, Oracle,
 * SQLite and ODBC qualify only `schema.table`, so a table in another database than the
 * execution database cannot be queried (browse-only). MySQL and SQL Server qualify the database.
 */
export function canTargetTable(connId: string, ref: TableRef): { ok: true } | { ok: false; reason: string } {
  const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId);
  if (conn && DB_QUALIFYING.has(conn.driver)) return { ok: true };
  const exec = execDatabase(connId);
  if (ref.database && exec && ref.database !== exec) return { ok: false, reason: ts('browseOnly', { db: exec }) };
  return { ok: true };
}
