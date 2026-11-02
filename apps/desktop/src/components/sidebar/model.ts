// Pure helpers of the sidebar item list (DESIGN.md §5.C "Sidebar"): sections by object kind,
// the search filter, the flat keyboard order and the row texts. No store access here.

import type { ExactCount, TableInfo, TableKind, TableRef } from 'ipc-types';
import { fuzzyMatch } from '@/lib/fuzzy';
import { formatBytes, formatCompact, formatInt, num } from '@/lib/format';
import { ts } from './strings';

export type SectionKey = 'table' | 'view' | 'materialized_view' | 'foreign' | 'other';

/** Display order; a section without rows is hidden. */
export const SECTION_ORDER: readonly SectionKey[] = ['table', 'view', 'materialized_view', 'foreign', 'other'];

/** Partitioned tables live in 테이블 (with their own icon). */
export function sectionOf(kind: TableKind): SectionKey {
  return kind === 'partitioned' ? 'table' : kind;
}

/** `ui.collapsedSections` key of a section. */
export const collapseKey = (key: SectionKey): string => `items:${key}`;

export function sectionTitle(key: SectionKey): string {
  switch (key) {
    case 'table':
      return ts('sectionTable');
    case 'view':
      return ts('sectionView');
    case 'materialized_view':
      return ts('sectionMatView');
    case 'foreign':
      return ts('sectionForeign');
    default:
      return ts('sectionOther');
  }
}

export interface ItemRow {
  table: TableInfo;
  /** Matched character positions of the name (search highlight). */
  indices: number[];
}

export interface ItemSection {
  key: SectionKey;
  rows: ItemRow[];
  /** Header collapsed (persisted); while searching every section shows its matches. */
  collapsed: boolean;
  /** Some row shows an estimate, so the header carries the `추정` caption. */
  hasEstimate: boolean;
}

export interface ItemModel {
  sections: ItemSection[];
  /** Rows in keyboard order (collapsed sections skipped). */
  visible: TableInfo[];
  /** Rows matching the search (all sections). */
  matchCount: number;
}

/**
 * Group `tables` by kind, keep the catalog order inside each section and drop rows that do not
 * match `query` (fuzzy over the name). `hasExact` tells which rows show an exact count instead
 * of the estimate.
 */
export function buildItemModel(
  tables: readonly TableInfo[],
  query: string,
  collapsed: Readonly<Record<string, boolean>>,
  hasExact: (t: TableInfo) => boolean,
): ItemModel {
  const q = query.trim();
  const buckets = new Map<SectionKey, ItemRow[]>();
  let matchCount = 0;
  for (const table of tables) {
    let indices: number[] = [];
    if (q) {
      const m = fuzzyMatch(q, table.name);
      if (!m) continue;
      indices = m.indices;
    }
    matchCount += 1;
    const key = sectionOf(table.kind);
    const list = buckets.get(key);
    if (list) list.push({ table, indices });
    else buckets.set(key, [{ table, indices }]);
  }
  const sections: ItemSection[] = [];
  const visible: TableInfo[] = [];
  for (const key of SECTION_ORDER) {
    const rows = buckets.get(key);
    if (!rows || rows.length === 0) continue;
    const isCollapsed = !q && !!collapsed[collapseKey(key)];
    const hasEstimate = rows.some((r) => !hasExact(r.table) && num(r.table.estimatedRows) !== undefined);
    sections.push({ key, rows, collapsed: isCollapsed, hasEstimate });
    if (!isCollapsed) for (const r of rows) visible.push(r.table);
  }
  return { sections, visible, matchCount };
}

/** The reference of a listed table in the browsed database. */
export function refOf(database: string | null, table: Pick<TableInfo, 'schema' | 'name'>): TableRef {
  return database ? { database, schema: table.schema, name: table.name } : { schema: table.schema, name: table.name };
}

export function sameRef(a: TableRef | null | undefined, b: TableRef): boolean {
  return !!a && a.name === b.name && (a.schema ?? '') === (b.schema ?? '') && (a.database ?? '') === (b.database ?? '');
}

const pad2 = (n: number) => String(n).padStart(2, '0');

/** `10-27` (row) or `2026-10-27 13:02` (tooltip) in local time. */
export function countDate(iso: string, full = false): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const md = `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  return full ? `${d.getFullYear()}-${md} ${pad2(d.getHours())}:${pad2(d.getMinutes())}` : md;
}

/**
 * Right-aligned row text: the exact count with its date, else `≈` + the compact estimate, else
 * the total size, else nothing.
 */
export function rowTrailing(table: TableInfo, exact: ExactCount | undefined): string | undefined {
  if (exact) return `${formatInt(exact.rows)} · ${countDate(exact.at)}`;
  const est = num(table.estimatedRows);
  if (est !== undefined) return `≈${formatCompact(est)}`;
  if (num(table.totalBytes) !== undefined) return formatBytes(table.totalBytes, 0);
  return undefined;
}

/** Multi-line row tooltip: `schema.name`, comment, the count (추정 / 정확) and the total size. */
export function rowTooltip(table: TableInfo, exact: ExactCount | undefined): string {
  const lines = [`${table.schema}.${table.name}`];
  if (table.comment) lines.push(table.comment);
  if (table.kind === 'partitioned') lines.push(ts('rowPartitioned'));
  const est = num(table.estimatedRows);
  if (exact) lines.push(ts('rowExactTip', { rows: formatInt(exact.rows), at: countDate(exact.at, true) }));
  else if (est !== undefined) lines.push(ts('rowEstimatedTip', { rows: formatInt(est) }));
  else lines.push(ts('rowNoEstimate'));
  if (num(table.totalBytes) !== undefined) lines.push(ts('rowSize', { size: formatBytes(table.totalBytes) }));
  return lines.join('\n');
}

/** Accessible row name: always says whether the count is estimated or exact. */
export function rowAriaLabel(table: TableInfo, exact: ExactCount | undefined): string {
  if (exact) return ts('ariaExact', { name: table.name, rows: formatInt(exact.rows), at: countDate(exact.at, true) });
  const est = num(table.estimatedRows);
  if (est !== undefined) return ts('ariaEstimated', { name: table.name, rows: formatInt(est) });
  return table.name;
}
