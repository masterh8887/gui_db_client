'use client';

// Workspace tabs (DESIGN.md §7.1 "tabs"): one tab set per connection (its workspace), with query
// tabs (each owns a `QueryDoc` in the extract store, same id), table tabs (Structure | Query
// view; a preview tab is replaced by the next preview) and one history tab per workspace.
// Closed tabs (≤ 10 per set) can be reopened with their document. Session state only.

import type { TableRef } from 'ipc-types';
import { create } from 'zustand';
import { catalogSelectSql } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { canTargetTable } from '@/lib/table-target';
import { useExtractStore, type QueryDoc, type QueryDocInit } from './extract';
import { logLocal } from './logs';
import { useScriptStore } from './script';
import { ts } from './strings';

export type TabKind = 'query' | 'table' | 'history' | 'script';
export type TableView = 'structure' | 'query';
export type StructureSegment = 'columns' | 'indexes' | 'ddl' | 'overview';

interface TabBase {
  id: string;
  connId: string;
  customTitle?: string;
}

export interface QueryTab extends TabBase {
  kind: 'query';
  /** "쿼리 {seq}". */
  seq: number;
}

export interface TableTab extends TabBase {
  kind: 'table';
  table: TableRef;
  preview: boolean;
  view: TableView;
  segment: StructureSegment;
}

/** One per workspace. */
export interface HistoryTab extends TabBase {
  kind: 'history';
}

export interface ScriptTab extends TabBase {
  kind: 'script';
  /** Sequential number, same as QueryTab. */
  seq: number;
}

export type WorkspaceTab = QueryTab | TableTab | HistoryTab | ScriptTab;

export interface ClosedTab {
  tab: WorkspaceTab;
  doc?: QueryDoc;
  index: number;
}

export interface TabSet {
  ids: string[];
  activeId: string | null;
  /** Most recently closed last; at most `CLOSED_LIMIT`. */
  closed: ClosedTab[];
}

export const CLOSED_LIMIT = 10;

export interface TabsState {
  nextId: number;
  tabs: Record<string, WorkspaceTab>;
  /** Key: connId. */
  sets: Record<string, TabSet>;
  /** The connection of the workspace on screen (`null` on Welcome); kept by the workspace store. */
  workspaceConnId: string | null;

  openQuery(connId: string, init?: QueryDocInit & { title?: string }): string;
  openScript(connId: string): string;
  openTable(
    connId: string,
    table: TableRef,
    opts?: { preview?: boolean; view?: TableView; segment?: StructureSegment; newTab?: boolean },
  ): string;
  openHistory(connId: string): string;
  close(id: string): void;
  closeOthers(id: string): void;
  closeToRight(id: string): void;
  closeAll(connId: string): void;
  reopenClosed(connId: string): string | null;
  activate(id: string): void;
  /** 0-based; -1 = last. */
  activateIndex(connId: string, index: number): void;
  activateNext(connId: string, delta: 1 | -1): void;
  reorder(connId: string, from: number, to: number): void;
  /** `null` / '' restores the default title. Renaming pins a preview tab. */
  rename(id: string, title: string | null): void;
  pin(id: string): void;
  setTableView(id: string, view: TableView): Promise<void>;
  setSegment(id: string, segment: StructureSegment): void;
  /** Dispose every tab and document of a connection's workspace. */
  dropConnection(connId: string): void;
  /** Called by the workspace store. */
  setWorkspaceConn(connId: string | null): void;
}

const EMPTY_SET: TabSet = { ids: [], activeId: null, closed: [] };

function sameTable(a: TableRef, b: TableRef): boolean {
  return a.name === b.name && (a.schema ?? '') === (b.schema ?? '') && (a.database ?? '') === (b.database ?? '');
}

/** Index right after the active tab (the end when nothing is active). */
function insertIndex(set: TabSet): number {
  const i = set.activeId ? set.ids.indexOf(set.activeId) : -1;
  return i < 0 ? set.ids.length : i + 1;
}

function withInserted(set: TabSet, id: string, at: number): TabSet {
  const ids = [...set.ids];
  ids.splice(Math.max(0, Math.min(at, ids.length)), 0, id);
  return { ...set, ids, activeId: id };
}

export const useTabsStore = create<TabsState>()((set, get) => {
  const getSet = (connId: string): TabSet => get().sets[connId] ?? EMPTY_SET;
  const newId = (prefix: 'q' | 't' | 'h'): string => {
    const n = get().nextId;
    set({ nextId: n + 1 });
    return `${prefix}${n}`;
  };
  const putSet = (connId: string, next: TabSet) => set((s) => ({ sets: { ...s.sets, [connId]: next } }));

  /** Remove tabs from their set (pushing them onto `closed` unless `discard`), dispose docs. */
  const removeTabs = (connId: string, ids: string[], discard = false) => {
    if (ids.length === 0) return;
    const s = get();
    const cur = getSet(connId);
    const extract = useExtractStore.getState();
    const closed = [...cur.closed];
    const remove = new Set(ids);
    if (!discard) {
      for (const id of ids) {
        const tab = s.tabs[id];
        if (!tab) continue;
        const doc = extract.docs[id];
        closed.push({ tab, doc: doc ? { ...doc, busy: null } : undefined, index: cur.ids.indexOf(id) });
      }
    }
    const remaining = cur.ids.filter((id) => !remove.has(id));
    let activeId = cur.activeId;
    if (activeId && remove.has(activeId)) {
      // The neighbour to the right of the closed active tab, else the one to its left.
      const oldIndex = cur.ids.indexOf(activeId);
      const right = cur.ids.slice(oldIndex + 1).find((id) => !remove.has(id));
      const left = [...cur.ids.slice(0, oldIndex)].reverse().find((id) => !remove.has(id));
      activeId = right ?? left ?? null;
    }
    const tabs = { ...s.tabs };
    for (const id of ids) delete tabs[id];
    set({
      tabs,
      sets: { ...s.sets, [connId]: { ids: remaining, activeId, closed: closed.slice(-CLOSED_LIMIT) } },
    });
    const script = useScriptStore.getState();
    for (const id of ids) {
      extract.disposeDoc(id);
      if (s.tabs[id]?.kind === 'script') script.disposeDoc(id);
    }
  };

  /** Create the Query-view doc of a table tab and fill it with the select-all SQL. */
  const loadSelectSql = async (tab: TableTab) => {
    const extract = useExtractStore.getState();
    const doc = extract.createDoc(tab.id, tab.connId, { name: tab.table.name });
    const gen = doc.gen;
    const patchDoc = (p: Partial<QueryDoc>) =>
      useExtractStore.setState((s) => {
        const d = s.docs[tab.id];
        if (!d || d.gen !== gen) return s;
        return { docs: { ...s.docs, [tab.id]: { ...d, ...p } } };
      });
    patchDoc({ busy: 'select' });
    try {
      const sql = await catalogSelectSql(tab.connId, tab.table);
      patchDoc({ sql, baseline: sql, busy: null });
    } catch (e) {
      const error = errorText(e);
      logLocal('error', ts('selectFailed', { error }));
      patchDoc({ error, busy: null });
    }
  };

  return {
    nextId: 1,
    tabs: {},
    sets: {},
    workspaceConnId: null,

    setWorkspaceConn: (connId) => set({ workspaceConnId: connId }),

    openQuery: (connId, init = {}) => {
      const { title, ...docInit } = init;
      const cur = getSet(connId);
      const s = get();
      const seq =
        cur.ids.reduce((max, id) => {
          const tab = s.tabs[id];
          return tab?.kind === 'query' ? Math.max(max, tab.seq) : max;
        }, 0) + 1;
      const id = newId('q');
      const tab: QueryTab = { id, kind: 'query', connId, seq, customTitle: title?.trim() || undefined };
      useExtractStore.getState().createDoc(id, connId, docInit);
      set((st) => ({ tabs: { ...st.tabs, [id]: tab } }));
      putSet(connId, withInserted(getSet(connId), id, insertIndex(getSet(connId))));
      return id;
    },

    openScript: (connId) => {
      const cur = getSet(connId);
      const s = get();
      const seq =
        cur.ids.reduce((max, id) => {
          const tab = s.tabs[id];
          return tab?.kind === 'script' ? Math.max(max, tab.seq) : max;
        }, 0) + 1;
      const id = newId('q');
      const tab: ScriptTab = { id, kind: 'script', connId, seq };
      useScriptStore.getState().createDoc(id, connId);
      set((st) => ({ tabs: { ...st.tabs, [id]: tab } }));
      putSet(connId, withInserted(getSet(connId), id, insertIndex(getSet(connId))));
      return id;
    },

    openTable: (connId, table, opts = {}) => {
      const cur = getSet(connId);
      const s = get();
      const segment = opts.segment ?? 'columns';
      const view = opts.view ?? 'structure';
      if (!opts.newTab) {
        const existing = cur.ids
          .map((id) => s.tabs[id])
          .find((tab): tab is TableTab => tab?.kind === 'table' && sameTable(tab.table, table));
        if (existing) {
          const next: TableTab = {
            ...existing,
            preview: opts.preview ? existing.preview : false,
            segment: opts.segment ?? existing.segment,
          };
          set((st) => ({ tabs: { ...st.tabs, [existing.id]: next } }));
          putSet(connId, { ...getSet(connId), activeId: existing.id });
          if (opts.view && opts.view !== existing.view) void get().setTableView(existing.id, opts.view);
          return existing.id;
        }
      }
      const preview = !!opts.preview && !opts.newTab;
      const id = newId('t');
      const tab: TableTab = { id, kind: 'table', connId, table: { ...table }, preview, view: 'structure', segment };
      if (preview) {
        const oldPreview = cur.ids.map((x) => s.tabs[x]).find((x): x is TableTab => x?.kind === 'table' && x.preview);
        if (oldPreview) {
          // Replace in place: new id, same index, not pushed onto `closed`.
          const index = cur.ids.indexOf(oldPreview.id);
          const tabs = { ...get().tabs, [id]: tab };
          delete tabs[oldPreview.id];
          const ids = [...cur.ids];
          ids[index] = id;
          set({ tabs });
          putSet(connId, { ...cur, ids, activeId: id });
          useExtractStore.getState().disposeDoc(oldPreview.id);
          return id;
        }
      }
      set((st) => ({ tabs: { ...st.tabs, [id]: tab } }));
      putSet(connId, withInserted(getSet(connId), id, insertIndex(getSet(connId))));
      if (view === 'query') void get().setTableView(id, 'query');
      return id;
    },

    openHistory: (connId) => {
      const cur = getSet(connId);
      const s = get();
      const existing = cur.ids.find((id) => s.tabs[id]?.kind === 'history');
      if (existing) {
        putSet(connId, { ...cur, activeId: existing });
        return existing;
      }
      const id = newId('h');
      set((st) => ({ tabs: { ...st.tabs, [id]: { id, kind: 'history', connId } } }));
      putSet(connId, withInserted(getSet(connId), id, insertIndex(getSet(connId))));
      return id;
    },

    close: (id) => {
      const tab = get().tabs[id];
      if (tab) removeTabs(tab.connId, [id]);
    },

    closeOthers: (id) => {
      const tab = get().tabs[id];
      if (!tab) return;
      removeTabs(
        tab.connId,
        getSet(tab.connId).ids.filter((x) => x !== id),
      );
      putSet(tab.connId, { ...getSet(tab.connId), activeId: id });
    },

    closeToRight: (id) => {
      const tab = get().tabs[id];
      if (!tab) return;
      const ids = getSet(tab.connId).ids;
      removeTabs(tab.connId, ids.slice(ids.indexOf(id) + 1));
    },

    closeAll: (connId) => removeTabs(connId, [...getSet(connId).ids]),

    reopenClosed: (connId) => {
      const cur = getSet(connId);
      const entry = cur.closed[cur.closed.length - 1];
      if (!entry) return null;
      const s = get();
      const closed = cur.closed.slice(0, -1);
      // One history tab per workspace: an open one wins.
      if (entry.tab.kind === 'history') {
        const open = cur.ids.find((id) => s.tabs[id]?.kind === 'history');
        if (open) {
          putSet(connId, { ...cur, closed, activeId: open });
          return open;
        }
      }
      let tab: WorkspaceTab = entry.tab;
      // At most one preview tab: a reopened preview comes back pinned when another exists.
      if (tab.kind === 'table' && tab.preview && cur.ids.some((id) => {
        const x = s.tabs[id];
        return x?.kind === 'table' && x.preview;
      })) {
        tab = { ...tab, preview: false };
      }
      set((st) => ({ tabs: { ...st.tabs, [tab.id]: tab } }));
      putSet(connId, withInserted({ ...cur, closed }, tab.id, entry.index));
      if (entry.doc) useExtractStore.getState().restoreDoc(entry.doc);
      return tab.id;
    },

    activate: (id) => {
      const tab = get().tabs[id];
      if (!tab) return;
      const cur = getSet(tab.connId);
      if (cur.activeId !== id) putSet(tab.connId, { ...cur, activeId: id });
    },

    activateIndex: (connId, index) => {
      const cur = getSet(connId);
      if (cur.ids.length === 0) return;
      const id = index < 0 ? cur.ids[cur.ids.length - 1] : cur.ids[index];
      if (id) putSet(connId, { ...cur, activeId: id });
    },

    activateNext: (connId, delta) => {
      const cur = getSet(connId);
      const n = cur.ids.length;
      if (n === 0) return;
      const i = cur.activeId ? cur.ids.indexOf(cur.activeId) : -1;
      const next = i < 0 ? 0 : (i + delta + n) % n;
      putSet(connId, { ...cur, activeId: cur.ids[next] });
    },

    reorder: (connId, from, to) => {
      const cur = getSet(connId);
      if (from === to || from < 0 || from >= cur.ids.length) return;
      const ids = [...cur.ids];
      const [moved] = ids.splice(from, 1);
      ids.splice(Math.max(0, Math.min(to, ids.length)), 0, moved);
      putSet(connId, { ...cur, ids });
    },

    rename: (id, title) => {
      const tab = get().tabs[id];
      if (!tab) return;
      const customTitle = title?.trim() || undefined;
      const next: WorkspaceTab = tab.kind === 'table' ? { ...tab, customTitle, preview: false } : { ...tab, customTitle };
      set((s) => ({ tabs: { ...s.tabs, [id]: next } }));
    },

    pin: (id) => {
      const tab = get().tabs[id];
      if (tab?.kind !== 'table' || !tab.preview) return;
      set((s) => ({ tabs: { ...s.tabs, [id]: { ...tab, preview: false } } }));
    },

    setTableView: async (id, view) => {
      const tab = get().tabs[id];
      if (tab?.kind !== 'table') return;
      if (view === 'structure') {
        if (tab.view !== 'structure') set((s) => ({ tabs: { ...s.tabs, [id]: { ...tab, view } } }));
        return;
      }
      const target = canTargetTable(tab.connId, tab.table);
      if (!target.ok) {
        logLocal('warn', target.reason);
        return;
      }
      const next: TableTab = { ...tab, view: 'query', preview: false };
      set((s) => ({ tabs: { ...s.tabs, [id]: next } }));
      if (!useExtractStore.getState().docs[id]) await loadSelectSql(next);
    },

    setSegment: (id, segment) => {
      const tab = get().tabs[id];
      if (tab?.kind !== 'table' || tab.segment === segment) return;
      set((s) => ({ tabs: { ...s.tabs, [id]: { ...tab, segment } } }));
    },

    dropConnection: (connId) => {
      const cur = get().sets[connId];
      if (!cur) return;
      const extract = useExtractStore.getState();
      const tabs = { ...get().tabs };
      for (const id of cur.ids) {
        delete tabs[id];
        extract.disposeDoc(id);
      }
      const sets = { ...get().sets };
      delete sets[connId];
      set({ tabs, sets });
    },
  };
});

// ---- selectors ---------------------------------------------------------------------------

const NO_TABS: WorkspaceTab[] = [];

/** Tabs of a workspace in order (use with `useShallow`). */
export function selectTabList(s: TabsState, connId: string | null): WorkspaceTab[] {
  if (!connId) return NO_TABS;
  const set = s.sets[connId];
  if (!set) return NO_TABS;
  return set.ids.map((id) => s.tabs[id]).filter((t): t is WorkspaceTab => !!t);
}

export function selectActiveTab(s: TabsState, connId: string | null): WorkspaceTab | undefined {
  if (!connId) return undefined;
  const id = s.sets[connId]?.activeId;
  return id ? s.tabs[id] : undefined;
}

/** The active tab when it edits SQL: a query tab, or a table tab in Query view. */
export function selectActiveQueryTarget(s: TabsState, connId: string | null): string | null {
  const tab = selectActiveTab(s, connId);
  if (!tab) return null;
  if (tab.kind === 'query') return tab.id;
  if (tab.kind === 'table' && tab.view === 'query') return tab.id;
  return null;
}

/** Whether a tab edits SQL (query tab, or table tab in Query view). */
export function isQueryTarget(tab: WorkspaceTab | undefined): boolean {
  return !!tab && (tab.kind === 'query' || (tab.kind === 'table' && tab.view === 'query'));
}

/** Reactive: the active query target of the workspace on screen. */
export function useActiveQueryTarget(): string | null {
  return useTabsStore((s) => selectActiveQueryTarget(s, s.workspaceConnId));
}

/** Tab strip title: query `customTitle ?? doc.name || "쿼리 N"`; table name; "기록". */
export function tabTitle(tab: WorkspaceTab, doc?: Pick<QueryDoc, 'name'>): string {
  if (tab.customTitle) return tab.customTitle;
  switch (tab.kind) {
    case 'query':
      return doc?.name.trim() || ts('queryTitle', { n: tab.seq });
    case 'table':
      return tab.table.name;
    case 'history':
      return ts('historyTitle');
    case 'script':
      return ts('scriptTitle', { n: tab.seq });
  }
}
