'use client';

// Small pieces shared by the structure view, the table inspector and the history views.

import type { ExactCount, TableKind, TableRef } from 'ipc-types';
import { Layers, Plug, Table2, View, type LucideProps } from 'lucide-react';
import { Tooltip } from '@/components/ui';
import { catalogSelectSql } from '@/lib/commands';
import { formatDateTime, formatDuration, formatInt, num } from '@/lib/format';
import { errorText } from '@/lib/ipc-error';
import { canTargetTable } from '@/lib/table-actions';
import { toast } from '@/lib/toast';
import { cn } from '@/lib/utils';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { ts } from './strings';

const pad2 = (n: number) => String(n).padStart(2, '0');

/**
 * Chips inside a selected row of a focused grid or list: the row turns accent-filled, so a soft
 * tinted chip would vanish; it becomes a translucent white chip with white text instead.
 */
export const SEL_CHIP =
  '[[data-roving-list]:focus-within_[aria-selected=true]_&]:bg-on-accent/20 [[data-roving-list]:focus-within_[aria-selected=true]_&]:text-on-accent [[data-roving-list]:focus-within_[aria-selected=true]_&]:shadow-none';

/**
 * Short local date and time: `10-27 13:02` in the current year, `2025-12-31 23:59` otherwise.
 * Invalid input is returned as-is.
 */
export function shortDateTime(iso: string | undefined | null, now: Date = new Date()): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const md = `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  return d.getFullYear() === now.getFullYear() ? `${md} ${hm}` : `${d.getFullYear()}-${md} ${hm}`;
}

export function kindLabel(kind: TableKind | undefined): string {
  return ts(`kind.${kind ?? 'table'}`);
}

/** The sidebar's icon for a table kind (materialized views: the accent `View`). */
export function KindIcon({ kind, className, ...rest }: { kind: TableKind | undefined } & LucideProps) {
  const p = { size: 14, strokeWidth: 1.75, 'aria-hidden': true, ...rest };
  switch (kind) {
    case 'partitioned':
      return <Layers {...p} className={cn('sel-muted shrink-0 text-fg-secondary', className)} />;
    case 'view':
      return <View {...p} className={cn('sel-muted shrink-0 text-fg-secondary', className)} />;
    case 'materialized_view':
      return <View {...p} className={cn('sel-muted shrink-0 text-accent', className)} />;
    case 'foreign':
      return <Plug {...p} className={cn('sel-muted shrink-0 text-fg-secondary', className)} />;
    default:
      return <Table2 {...p} className={cn('sel-muted shrink-0 text-fg-secondary', className)} />;
  }
}

/**
 * An exact count with its date: `48,213,004 · 10-27 13:02`; the tooltip says when it was
 * counted and how long it took, the accessible name says "exact".
 */
export function ExactCountValue({ exact, className }: { exact: ExactCount; className?: string }) {
  const rows = formatInt(exact.rows);
  const at = formatDateTime(exact.at, true);
  return (
    <Tooltip content={ts('ov.countedTip', { at, took: formatDuration(num(exact.elapsedMs)) })}>
      <span
        aria-label={ts('ov.exactAria', { rows, at })}
        data-exact-count=""
        className={cn('inline-flex min-w-0 items-baseline gap-1.5 whitespace-nowrap tabular-nums', className)}
      >
        <span className="font-medium">{rows}</span>
        <span className="sel-muted text-sm text-fg-secondary">· {shortDateTime(exact.at)}</span>
      </span>
    </Tooltip>
  );
}

/**
 * Whether the table's details (`key` = `tableKey(connId, ref)`) are absent from the catalog
 * store with no load in flight and no error: true after a catalog refresh dropped them
 * (`clearConnection`), so an open view loads them again. False for a connection that is no
 * longer saved (deleted: its tabs are about to close), and after a failed load (no retry loop).
 */
export function useDetailsMissing(connId: string, key: string): boolean {
  const known = useConnectionsStore((s) => s.connections.some((c) => c.id === connId));
  const missing = useCatalogStore((s) => !s.details[key] && !s.loading[key] && !s.errors[key]);
  return known && missing;
}

/**
 * Open a new query tab on `connId` holding the table's select-all SQL (`catalog_select_sql`),
 * named after the table. Refused with the `canTargetTable` reason for a browse-only table.
 */
export async function openSelectAllInNewTab(connId: string, ref: TableRef): Promise<string | null> {
  const target = canTargetTable(connId, ref);
  if (!target.ok) {
    toast(target.reason, { tone: 'danger' });
    return null;
  }
  try {
    const sql = await catalogSelectSql(connId, ref);
    return useTabsStore.getState().openQuery(connId, { sql, name: ref.name });
  } catch (e) {
    logLocal('error', `catalog_select_sql: ${errorText(e)}`);
    toast(ts('selectSqlFailed', { error: errorText(e) }), { tone: 'danger' });
    return null;
  }
}
