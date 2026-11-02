// Result sources of the command palette (DESIGN.md §5.H), kept pure so the grouping, scoping and
// empty-query rules are testable without rendering: tables of the current browse scope,
// connections, history (this connection first) and actions (`getActions()` where enabled).
// Nothing here ever carries table data: only names, estimates, hosts, labels and times.

import type { Connection, ExactCount, HistoryEntry, TableInfo, TableRef } from 'ipc-types';
import type { AppAction } from '@/lib/actions';
import { formatDateTime } from '@/lib/format';
import { fuzzyMatch } from '@/lib/fuzzy';
import { tableKey } from '@/lib/utils';
import type { PaletteScope } from '@/stores/ui';
import { ps } from './strings';

interface ItemBase {
  /** Unique within the result list (also the option's DOM id suffix). */
  id: string;
  title: string;
  secondary: string;
  /** Matched characters of `title` (for `Highlight`). */
  indices: number[];
  score: number;
}

export interface TableItem extends ItemBase {
  kind: 'table';
  connId: string;
  ref: TableRef;
  info?: TableInfo;
  exact?: ExactCount;
}
export interface ConnectionItem extends ItemBase {
  kind: 'connection';
  conn: Connection;
  opened: boolean;
  current: boolean;
}
export interface HistoryItem extends ItemBase {
  kind: 'history';
  entry: HistoryEntry;
}
export interface ActionItem extends ItemBase {
  kind: 'action';
  action: AppAction;
}
export type PaletteItem = TableItem | ConnectionItem | HistoryItem | ActionItem;

export type GroupId = 'tables' | 'connections' | 'history' | 'actions';
export interface PaletteGroup {
  id: GroupId;
  title: string;
  items: PaletteItem[];
}

export interface PaletteInput {
  mode: 'welcome' | 'workspace';
  /** The workspace on screen (`null` on Welcome). */
  connId: string | null;
  /** The scope chip. */
  scope: PaletteScope;
  /** Raw input text (may start with `>` or `@`). */
  query: string;
  /** Pick mode (`openInConnection`): the connection the query comes from is left out. */
  pick?: { fromConnId: string | null };
  /** Tables of the current browse scope (database / schema) of `connId`. */
  tables: { database: string | null; list: readonly TableInfo[] } | null;
  exactCounts: Readonly<Record<string, ExactCount>>;
  /** Tables opened this session on this connection, most recent first. */
  recentTables: readonly TableRef[];
  findTable?: (ref: TableRef) => TableInfo | undefined;
  connections: readonly Connection[];
  /** In-memory workspaces, most recently used first. */
  opened: readonly string[];
  /** History entries, this connection's first. */
  history: readonly HistoryEntry[];
  /** Enabled, non-hidden actions in registration order. */
  actions: readonly AppAction[];
  /** Actions run from the palette this session, most recent first. */
  recentActionIds: readonly string[];
}

const LIMITS: Record<GroupId, number> = { tables: 40, connections: 10, history: 10, actions: 20 };
const EMPTY_LIMITS = { recentTables: 8, connections: 10, recentActions: 8, scoped: 50 };
/** Score penalty of a match outside the title (host, SQL, keywords): never above a title hit. */
const EXTRA_PENALTY = 60;

/** Scope and search text after the `>` (actions) / `@` (connections) prefixes. */
export function parseQuery(scope: PaletteScope, raw: string): { scope: PaletteScope; text: string; prefix: string } {
  if (scope === 'all') {
    const m = /^([>@])\s*/.exec(raw);
    if (m) return { scope: m[1] === '>' ? 'actions' : 'connections', text: raw.slice(m[0].length), prefix: m[0] };
  }
  return { scope, text: raw, prefix: '' };
}

function match(q: string, title: string, extras: readonly (string | undefined)[] = []): { score: number; indices: number[] } | null {
  if (!q) return { score: 0, indices: [] };
  const m = fuzzyMatch(q, title);
  if (m) return m;
  let best: number | null = null;
  for (const x of extras) {
    if (!x) continue;
    const e = fuzzyMatch(q, x, 'contains');
    if (e && (best === null || e.score > best)) best = e.score;
  }
  return best === null ? null : { score: best - EXTRA_PENALTY, indices: [] };
}

/** `host:port · database`, or the SQLite path. */
export function connectionSubtitle(c: Connection): string {
  const path = c.fields.path;
  if (c.driver === 'sqlite' && path) return path;
  const host = c.host ? (c.port ? `${c.host}:${c.port}` : c.host) : '';
  return [host, c.database].filter(Boolean).join(' · ');
}

/** History entry title: its label, else the first line of its SQL. */
export function historyTitle(e: HistoryEntry): string {
  if (e.label) return e.label;
  const line = (e.sql || e.sqlFile || '').trim().split('\n')[0] ?? '';
  return line.length > 80 ? `${line.slice(0, 79)}…` : line;
}

function tableItem(inp: PaletteInput, connId: string, ref: TableRef, info: TableInfo | undefined, q: string): TableItem | null {
  const m = match(q, ref.name, [ref.schema ? `${ref.schema}.${ref.name}` : undefined]);
  if (!m) return null;
  const key = tableKey(connId, ref);
  return {
    kind: 'table',
    id: `table:${key}`,
    title: ref.name,
    secondary: ref.schema ?? '',
    connId,
    ref,
    info,
    exact: inp.exactCounts[key],
    ...m,
  };
}

function connectionItem(inp: PaletteInput, c: Connection, q: string): ConnectionItem | null {
  const m = match(q, c.name, [c.host, c.database, c.driver, c.tag, c.fields.path]);
  if (!m) return null;
  return {
    kind: 'connection',
    id: `conn:${c.id}`,
    title: c.name,
    secondary: connectionSubtitle(c),
    conn: c,
    opened: inp.opened.includes(c.id),
    current: c.id === inp.connId,
    ...m,
  };
}

function historyItem(inp: PaletteInput, e: HistoryEntry, q: string, names: Map<string, string>): HistoryItem | null {
  const title = historyTitle(e);
  const m = match(q, title, [e.sql, e.sqlFile, ...e.files]);
  if (!m) return null;
  const when = formatDateTime(e.startedAt);
  const other = e.connId !== inp.connId ? names.get(e.connId) ?? e.connId : null;
  return { kind: 'history', id: `hist:${String(e.id)}`, title, secondary: other ? `${when} · ${other}` : when, entry: e, ...m };
}

function actionItem(a: AppAction, q: string): ActionItem | null {
  const title = a.title();
  const m = match(q, title, [...(a.keywords ?? []), a.id]);
  if (!m) return null;
  return { kind: 'action', id: `action:${a.id}`, title, secondary: '', action: a, ...m };
}

function collect<T extends PaletteItem>(list: readonly (T | null)[], q: string, limit: number): T[] {
  const items = list.filter((x): x is T => x !== null);
  if (q) items.sort((a, b) => b.score - a.score);
  return items.slice(0, limit);
}

/** Connections with the open workspaces first (MRU), then the rest in list order. */
function openedFirst(inp: PaletteInput, list: readonly Connection[]): Connection[] {
  const byId = new Map(list.map((c) => [c.id, c]));
  const first = inp.opened.map((id) => byId.get(id)).filter((c): c is Connection => !!c);
  return [...first, ...list.filter((c) => !inp.opened.includes(c.id))];
}

/** The grouped results for the palette's current input. */
export function buildGroups(inp: PaletteInput): PaletteGroup[] {
  const { scope, text } = parseQuery(inp.scope, inp.query);
  const q = text.trim();
  const ws = inp.mode === 'workspace' && inp.connId !== null;
  const names = new Map(inp.connections.map((c) => [c.id, c.name]));

  if (inp.pick) {
    const others = openedFirst(
      inp,
      inp.connections.filter((c) => c.id !== inp.pick?.fromConnId),
    );
    const items = collect(others.map((c) => connectionItem(inp, c, q)), q, q ? LIMITS.connections : EMPTY_LIMITS.scoped);
    return items.length > 0 ? [{ id: 'connections', title: ps('groupPick'), items }] : [];
  }

  // Welcome offers connections and actions only.
  const wants = (g: GroupId) => (scope === 'all' ? ws || g === 'connections' || g === 'actions' : scope === g && (ws || g === 'connections' || g === 'actions'));
  const groups: PaletteGroup[] = [];
  const connId = inp.connId;

  if (!q) {
    if (wants('tables') && connId) {
      if (scope === 'tables') {
        const items = (inp.tables?.list ?? [])
          .slice(0, EMPTY_LIMITS.scoped)
          .map((t) => tableItem(inp, connId, { database: inp.tables?.database ?? undefined, schema: t.schema, name: t.name }, t, ''))
          .filter((x): x is TableItem => x !== null);
        if (items.length) groups.push({ id: 'tables', title: ps('groupTables'), items });
      } else {
        const items = inp.recentTables
          .slice(0, EMPTY_LIMITS.recentTables)
          .map((ref) => tableItem(inp, connId, ref, inp.findTable?.(ref), ''))
          .filter((x): x is TableItem => x !== null);
        if (items.length) groups.push({ id: 'tables', title: ps('groupRecentTables'), items });
      }
    }
    if (wants('connections')) {
      let list: Connection[];
      let title = ps('groupConnections');
      if (scope === 'connections' || !ws) list = openedFirst(inp, inp.connections);
      else {
        list = openedFirst(inp, inp.connections).filter((c) => inp.opened.includes(c.id) && c.id !== connId);
        title = ps('groupOpenConnections');
      }
      const limit = scope === 'connections' ? EMPTY_LIMITS.scoped : EMPTY_LIMITS.connections;
      const items = list.slice(0, limit).map((c) => connectionItem(inp, c, '')).filter((x): x is ConnectionItem => x !== null);
      if (items.length) groups.push({ id: 'connections', title, items });
    }
    if (wants('history') && scope === 'history') {
      const items = inp.history
        .slice(0, EMPTY_LIMITS.scoped)
        .map((e) => historyItem(inp, e, '', names))
        .filter((x): x is HistoryItem => x !== null);
      if (items.length) groups.push({ id: 'history', title: ps('groupHistory'), items });
    }
    if (wants('actions')) {
      const byId = new Map(inp.actions.map((a) => [a.id, a]));
      const recent = inp.recentActionIds.map((id) => byId.get(id)).filter((a): a is AppAction => !!a);
      const useRecent = scope !== 'actions' && recent.length > 0;
      const list = useRecent ? recent.slice(0, EMPTY_LIMITS.recentActions) : inp.actions;
      const items = list.map((a) => actionItem(a, '')).filter((x): x is ActionItem => x !== null);
      if (items.length) groups.push({ id: 'actions', title: useRecent ? ps('groupRecentActions') : ps('groupActions'), items });
    }
    return groups;
  }

  if (wants('tables') && connId && inp.tables) {
    const db = inp.tables.database ?? undefined;
    const items = collect(
      inp.tables.list.map((t) => tableItem(inp, connId, { database: db, schema: t.schema, name: t.name }, t, q)),
      q,
      LIMITS.tables,
    );
    if (items.length) groups.push({ id: 'tables', title: ps('groupTables'), items });
  }
  if (wants('connections')) {
    const items = collect(inp.connections.map((c) => connectionItem(inp, c, q)), q, LIMITS.connections);
    if (items.length) groups.push({ id: 'connections', title: ps('groupConnections'), items });
  }
  if (wants('history')) {
    const items = collect(inp.history.map((e) => historyItem(inp, e, q, names)), q, LIMITS.history);
    if (items.length) groups.push({ id: 'history', title: ps('groupHistory'), items });
  }
  if (wants('actions')) {
    const items = collect(inp.actions.map((a) => actionItem(a, q)), q, LIMITS.actions);
    if (items.length) groups.push({ id: 'actions', title: ps('groupActions'), items });
  }
  // Best group first; ties keep the order above.
  return groups
    .map((g, i) => ({ g, i, top: g.items[0]?.score ?? -Infinity }))
    .sort((a, b) => b.top - a.top || a.i - b.i)
    .map(({ g }) => g);
}
