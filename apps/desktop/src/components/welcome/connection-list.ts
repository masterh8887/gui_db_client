// Pure helpers of the Welcome list (DESIGN.md §5.A): fuzzy search over name, host, database,
// driver and tag; grouping by tag or driver; the second row line.

import type { Connection, DriverInfo } from 'ipc-types';
import { connTag, ENV_GROUP_ORDER, envOptionLabel, noTagLabel, type EnvTag } from '@/lib/env';
import { fuzzyMatch } from '@/lib/fuzzy';
import type { WelcomeGroupBy } from '@/stores/ui';
import { ws } from './strings';

export interface ConnMatch {
  conn: Connection;
  score: number;
  /** Matched positions in `conn.name` (empty when another field matched). */
  nameIndices: number[];
}

export interface ConnGroup {
  key: string;
  /** Header text; `null` = no header (name order). */
  label: string | null;
  tag?: EnvTag;
  items: ConnMatch[];
}

const byName = (a: ConnMatch, b: ConnMatch) =>
  a.conn.name.localeCompare(b.conn.name, undefined, { sensitivity: 'base', numeric: true }) || a.conn.id.localeCompare(b.conn.id);

function searchFields(c: Connection, driver: DriverInfo | undefined): string[] {
  return [c.host, c.database, c.fields.path, c.fields.service_name, c.driver, driver?.name, connTag(c)].filter(
    (v): v is string => !!v,
  );
}

/**
 * Connections matching `query` (all of them when it is blank), each with its best score over
 * the searchable fields. The name counts slightly more than the other fields.
 */
export function matchConnections(connections: readonly Connection[], drivers: readonly DriverInfo[], query: string): ConnMatch[] {
  const q = query.trim();
  if (!q) return connections.map((conn) => ({ conn, score: 0, nameIndices: [] }));
  const out: ConnMatch[] = [];
  for (const conn of connections) {
    const driver = drivers.find((d) => d.id === conn.driver);
    const onName = fuzzyMatch(q, conn.name);
    let best = onName ? onName.score + 5 : -Infinity;
    for (const field of searchFields(conn, driver)) {
      const m = fuzzyMatch(q, field);
      if (m && m.score > best) best = m.score;
    }
    if (best === -Infinity) continue;
    out.push({ conn, score: best, nameIndices: onName && onName.score + 5 >= best ? onName.indices : [] });
  }
  return out;
}

/** The best match (highest score, then name order), or undefined. */
export function bestMatch(matches: readonly ConnMatch[]): ConnMatch | undefined {
  let best: ConnMatch | undefined;
  for (const m of matches) {
    if (!best || m.score > best.score || (m.score === best.score && byName(m, best) < 0)) best = m;
  }
  return best;
}

/**
 * Display groups. `none`: one header-less group in name order (score order while searching, so
 * the best hit is on top); `tag`: production, staging, testing, development, local, no tag;
 * `driver`: by driver name. Items inside a group are in name order; empty groups are dropped.
 */
export function groupConnections(
  matches: readonly ConnMatch[],
  groupBy: WelcomeGroupBy,
  drivers: readonly DriverInfo[],
  searching: boolean,
): ConnGroup[] {
  if (groupBy === 'none') {
    const items = [...matches].sort(searching ? (a, b) => b.score - a.score || byName(a, b) : byName);
    return items.length ? [{ key: 'all', label: null, items }] : [];
  }
  if (groupBy === 'tag') {
    const groups: ConnGroup[] = ENV_GROUP_ORDER.map((tag) => ({ key: `tag:${tag}`, label: envOptionLabel(tag), tag, items: [] }));
    const untagged: ConnGroup = { key: 'tag:none', label: noTagLabel(), items: [] };
    for (const m of matches) {
      const tag = connTag(m.conn);
      (groups.find((g) => g.tag === tag) ?? untagged).items.push(m);
    }
    return [...groups, untagged].filter((g) => g.items.length > 0).map((g) => ({ ...g, items: g.items.sort(byName) }));
  }
  const byDriver = new Map<string, ConnGroup>();
  for (const m of matches) {
    const id = m.conn.driver;
    let g = byDriver.get(id);
    if (!g) {
      g = { key: `driver:${id}`, label: drivers.find((d) => d.id === id)?.name ?? id, items: [] };
      byDriver.set(id, g);
    }
    g.items.push(m);
  }
  return [...byDriver.values()]
    .sort((a, b) => (a.label ?? '').localeCompare(b.label ?? '', undefined, { sensitivity: 'base' }))
    .map((g) => ({ ...g, items: g.items.sort(byName) }));
}

/**
 * Name order (`none`) with connections open in any window of this process (`openIds`): a
 * "열린 연결" group first, then the others under "다른 연결" (docs/db-copy.md 1.1). Unchanged
 * while searching (score order), when nothing is open or for the other groupings.
 */
export function withOpenGroup(groups: readonly ConnGroup[], groupBy: WelcomeGroupBy, searching: boolean, openIds: ReadonlySet<string>): ConnGroup[] {
  const all = groups[0];
  if (groupBy !== 'none' || searching || openIds.size === 0 || groups.length !== 1 || !all) return [...groups];
  const open = all.items.filter((m) => openIds.has(m.conn.id));
  if (open.length === 0) return [...groups];
  const rest = all.items.filter((m) => !openIds.has(m.conn.id));
  const out: ConnGroup[] = [{ key: 'open', label: ws('groupOpen'), items: open }];
  if (rest.length > 0) out.push({ key: 'rest', label: ws('groupOthers'), items: rest });
  return out;
}

/** Rows in display order (the listbox's flat index space). */
export function flattenGroups(groups: readonly ConnGroup[]): Connection[] {
  return groups.flatMap((g) => g.items.map((m) => m.conn));
}

/**
 * Second row line: `host:port · database · SSH user@host`; SQLite: the file path (`path: true`,
 * shown middle-truncated); ODBC without a host: the ODBC driver name. Connection strings are
 * never shown (they may carry credentials).
 */
export function connSubtitle(c: Connection): { text: string; path: boolean } {
  if (c.fields.path) return { text: c.fields.path, path: true };
  const parts: string[] = [];
  if (c.host) parts.push(c.port ? `${c.host}:${c.port}` : c.host);
  else if (c.fields.driver) parts.push(c.fields.driver);
  const db = c.database || c.fields.service_name;
  if (db) parts.push(db);
  if (c.ssh?.enabled && c.ssh.host) parts.push(ws('ssh', { target: c.ssh.user ? `${c.ssh.user}@${c.ssh.host}` : c.ssh.host }));
  return { text: parts.join(' · '), path: false };
}
