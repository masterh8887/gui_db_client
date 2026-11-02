'use client';

// App mode and workspaces (DESIGN.md §7.1 "workspace"): Welcome vs one open connection's
// workspace. Workspaces stay in memory for the session (tabs and SQL survive switching); each
// has a browse scope (database + schema shown in the sidebar) and a sidebar selection.

import type { HistoryEntry, TableRef } from 'ipc-types';
import { create } from 'zustand';
import { askConfirm } from '@/lib/confirm';
import { toast } from '@/lib/toast';
import { useCatalogStore } from './catalog';
import { needsSessionPassword, useConnectionsStore } from './connections';
import { isDocDirty, useExtractStore } from './extract';
import { useSettingsStore } from './settings';
import { ts } from './strings';
import { useTabsStore } from './tabs';
import { useUiStore } from './ui';

export type AppMode = 'welcome' | 'workspace';

export interface BrowseScope {
  database: string | null;
  schema: string | null;
}

export interface WorkspaceState {
  mode: AppMode;
  /** The workspace on screen (kept while Welcome shows, so it can be resumed). */
  openConnId: string | null;
  /** In-memory workspaces, most recently used first. */
  opened: string[];
  scope: Record<string, BrowseScope>;
  selection: Record<string, TableRef | null>;

  /**
   * Show `connId`'s workspace. Resolves `true` once the mode switched (without waiting for the
   * catalog or the connection test); `false` for an unknown connection or while its password
   * is being asked for (the prompt reopens it).
   */
  openWorkspace(connId: string, opts?: { skipEmptyTab?: boolean }): Promise<boolean>;
  /** Back to Welcome; workspaces stay in memory. */
  showWelcome(): void;
  /** Drop a workspace (asks when a query has unsaved changes unless `force`). */
  closeWorkspace(connId: string, opts?: { force?: boolean }): Promise<boolean>;
  /** Make sure the browse scope is valid (database, then schema; see `pickSchema`). */
  ensureScope(connId: string): Promise<void>;
  /** Change database and/or schema; a new database clears the schema and picks one again. */
  setScope(connId: string, patch: Partial<BrowseScope>): void;
  select(connId: string, ref: TableRef | null): void;
  /** Open a history entry in a new query tab of its connection's workspace (and run it). */
  openFromHistory(entry: HistoryEntry, opts?: { run?: boolean }): Promise<string | null>;
}

const DEFAULT_SCHEMA: Record<string, string> = { postgres: 'public', postgresql: 'public', mssql: 'dbo', sqlite: 'main' };

/** Schema rule: the user's own schema, else the driver default if present, else the first. */
export function pickSchema(schemas: readonly { name: string }[], user: string | undefined, driver: string): string | null {
  if (schemas.length === 0) return null;
  const own = user ? schemas.find((s) => s.name.toLowerCase() === user.toLowerCase()) : undefined;
  if (own) return own.name;
  const def = DEFAULT_SCHEMA[driver];
  if (def && schemas.some((s) => s.name === def)) return def;
  return schemas[0].name;
}

/** Ongoing scope resolutions per connection; a newer one makes older results stale. */
const scopeTokens = new Map<string, number>();
const bumpToken = (connId: string) => {
  const n = (scopeTokens.get(connId) ?? 0) + 1;
  scopeTokens.set(connId, n);
  return n;
};

export const useWorkspaceStore = create<WorkspaceState>()((set, get) => {
  const findConn = (connId: string) => useConnectionsStore.getState().connections.find((c) => c.id === connId);
  const writeScope = (connId: string, scope: BrowseScope) => set((s) => ({ scope: { ...s.scope, [connId]: scope } }));

  /** Pick (or keep) the schema of `database` and store the scope, unless a newer resolution started. */
  const resolveSchema = async (connId: string, database: string, keep: string | null, token: number) => {
    const conn = findConn(connId);
    const schemas = await useCatalogStore.getState().loadSchemas(connId, database);
    if (scopeTokens.get(connId) !== token) return;
    const schema = keep && schemas.some((s) => s.name === keep) ? keep : pickSchema(schemas, conn?.user, conn?.driver ?? '');
    writeScope(connId, { database, schema });
  };

  return {
    mode: 'welcome',
    openConnId: null,
    opened: [],
    scope: {},
    selection: {},

    openWorkspace: async (connId, opts) => {
      const cs = useConnectionsStore.getState();
      if (!cs.connections.some((c) => c.id === connId)) {
        toast(ts('connNotFound'), { tone: 'danger' });
        return false;
      }
      if (needsSessionPassword(cs, connId)) {
        useUiStore.getState().askDbPassword(connId, 'open');
        return false;
      }
      // Synchronously, before any await: clicking A then B lands on B.
      set((s) => ({ mode: 'workspace', openConnId: connId, opened: [connId, ...s.opened.filter((x) => x !== connId)] }));
      useTabsStore.getState().setWorkspaceConn(connId);
      cs.select(connId);

      await useSettingsStore.getState().ensureLoaded();
      // Only per-connection state from here on.
      const tabs = useTabsStore.getState();
      if (!opts?.skipEmptyTab && (tabs.sets[connId]?.ids.length ?? 0) === 0) tabs.openQuery(connId);

      void useCatalogStore
        .getState()
        .loadDatabases(connId)
        .then(() => get().ensureScope(connId))
        .catch(() => undefined);
      if (useConnectionsStore.getState().status[connId] !== 'ok') {
        void useConnectionsStore.getState().test(connId).catch(() => undefined);
      }
      return true;
    },

    showWelcome: () => {
      set({ mode: 'welcome' });
      useTabsStore.getState().setWorkspaceConn(null);
    },

    closeWorkspace: async (connId, opts) => {
      if (!opts?.force) {
        const tabs = useTabsStore.getState();
        const docs = useExtractStore.getState().docs;
        const dirty = (tabs.sets[connId]?.ids ?? []).filter((id) => docs[id] && isDocDirty(docs[id])).length;
        if (dirty > 0) {
          const name = findConn(connId)?.name ?? connId;
          const ok = await askConfirm({
            title: ts('closeDirtyTitle', { name }),
            body: ts('closeDirtyBody', { count: dirty }),
            confirmLabel: ts('closeDirtyConfirm'),
            danger: true,
          });
          if (!ok) return false;
        }
      }
      useTabsStore.getState().dropConnection(connId);
      bumpToken(connId);
      const wasOpen = get().openConnId === connId;
      set((s) => {
        const scope = { ...s.scope };
        const selection = { ...s.selection };
        delete scope[connId];
        delete selection[connId];
        return {
          scope,
          selection,
          opened: s.opened.filter((x) => x !== connId),
          ...(wasOpen ? { mode: 'welcome' as const, openConnId: null } : {}),
        };
      });
      if (wasOpen) useTabsStore.getState().setWorkspaceConn(null);
      return true;
    },

    ensureScope: async (connId) => {
      const token = bumpToken(connId);
      const conn = findConn(connId);
      const dbs = await useCatalogStore.getState().loadDatabases(connId);
      if (scopeTokens.get(connId) !== token) return;
      const usable = dbs.filter((d) => d.accessible);
      const cur = get().scope[connId];
      let database: string | null = null;
      if (cur?.database && usable.some((d) => d.name === cur.database)) database = cur.database;
      else if (conn?.database && usable.some((d) => d.name === conn.database)) database = conn.database;
      else database = usable.find((d) => d.isCurrent)?.name ?? usable[0]?.name ?? null;
      if (!database) {
        writeScope(connId, { database: null, schema: null });
        return;
      }
      const keep = cur?.database === database ? cur.schema : null;
      if (cur?.database !== database) writeScope(connId, { database, schema: null });
      await resolveSchema(connId, database, keep, token);
    },

    setScope: (connId, patch) => {
      const cur = get().scope[connId] ?? { database: null, schema: null };
      if (patch.database !== undefined && patch.database !== cur.database) {
        const token = bumpToken(connId);
        writeScope(connId, { database: patch.database, schema: null });
        if (patch.database) void resolveSchema(connId, patch.database, patch.schema ?? null, token).catch(() => undefined);
        return;
      }
      if (patch.schema !== undefined) {
        bumpToken(connId);
        writeScope(connId, { ...cur, schema: patch.schema });
      }
    },

    select: (connId, ref) => set((s) => ({ selection: { ...s.selection, [connId]: ref } })),

    openFromHistory: async (entry, opts) => {
      const cs = useConnectionsStore.getState();
      if (!cs.connections.some((c) => c.id === entry.connId)) {
        toast(ts('connNotFound'), { tone: 'danger' });
        return null;
      }
      if (needsSessionPassword(cs, entry.connId)) {
        // The prompt opens the workspace, then this finishes the job.
        useUiStore.getState().askDbPassword(entry.connId, 'open', { then: () => void get().openFromHistory(entry, opts) });
        return null;
      }
      const ok = await get().openWorkspace(entry.connId, { skipEmptyTab: true });
      if (!ok) return null;
      const tabId = useTabsStore.getState().openQuery(entry.connId, {
        sql: entry.sql,
        sqlMode: entry.sqlFile ? 'file' : 'text',
        sqlFile: entry.sqlFile ?? '',
        name: entry.label,
        session: { ...entry.session },
        output: { ...entry.output },
      });
      if (opts?.run) void useExtractStore.getState().run(tabId);
      return tabId;
    },
  };
});

/** Reset module state between tests (scope tokens). */
export function __resetWorkspaceForTests(): void {
  scopeTokens.clear();
}
