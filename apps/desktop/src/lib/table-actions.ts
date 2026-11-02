'use client';

// Table actions shared by the sidebar, the structure view, the inspector and the palette
// (DESIGN.md §7.5). Every helper logs what it did (`logLocal('info')`); failures go to the log.

import type { TableRef } from 'ipc-types';
import { catalogColumns, catalogDdl, catalogExactCount } from '@/lib/commands';
import { useCatalogStore } from '@/stores/catalog';
import { isTerminalState, useJobsStore } from '@/stores/jobs';
import { logLocal } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { useWorkspaceStore } from '@/stores/workspace';
import { askConfirm } from './confirm';
import { formatCompact, num } from './format';
import { errorText } from './ipc-error';
import { ls } from './strings';
import { canTargetTable } from './table-target';
import { toast } from './toast';
import { copyText, tableKey, tableLabel } from './utils';

export { canTargetTable, execDatabase, useExecDatabase } from './table-target';

/** Estimates at or above this (or unknown) ask before an exact count. */
export const COUNT_CONFIRM_ROWS = 10_000_000;

/**
 * Open `ref` in a pinned table tab showing the Query view (the ready select-all SQL). Refused
 * (toast with the reason, `null`) when `canTargetTable` says no. Opens the workspace first when
 * another one is on screen.
 */
export async function openExtractionQuery(connId: string, ref: TableRef): Promise<string | null> {
  const target = canTargetTable(connId, ref);
  if (!target.ok) {
    toast(target.reason, { tone: 'danger' });
    return null;
  }
  const ws = useWorkspaceStore.getState();
  if (ws.mode !== 'workspace' || ws.openConnId !== connId) {
    if (!(await ws.openWorkspace(connId))) return null;
  }
  const tabs = useTabsStore.getState();
  const id = tabs.openTable(connId, ref, { preview: false });
  await useTabsStore.getState().setTableView(id, 'query');
  return id;
}

async function columnNames(connId: string, ref: TableRef): Promise<string[]> {
  const cached = useCatalogStore.getState().details[tableKey(connId, ref)]?.columns;
  const cols = cached ?? (await catalogColumns(connId, ref));
  return cols.map((c) => c.name);
}

/** Copy `col1, col2, …`; resolves whether the clipboard accepted it. */
export async function copyColumnList(connId: string, ref: TableRef): Promise<boolean> {
  try {
    const ok = await copyText((await columnNames(connId, ref)).join(', '));
    logLocal(ok ? 'info' : 'warn', ok ? ls('copiedColumns', { table: tableLabel(ref) }) : ls('copyFailed'));
    return ok;
  } catch (e) {
    logLocal('error', `catalog_columns: ${errorText(e)}`);
    return false;
  }
}

/** Copy `schema.name` (or `name` with `qualified = false`). */
export async function copyTableName(ref: TableRef, qualified = true): Promise<boolean> {
  const text = qualified ? tableLabel(ref) : ref.name;
  const ok = await copyText(text);
  logLocal(ok ? 'info' : 'warn', ok ? ls('copiedName', { table: text }) : ls('copyFailed'));
  return ok;
}

export async function copyDdl(connId: string, ref: TableRef): Promise<boolean> {
  try {
    const ddl = useCatalogStore.getState().details[tableKey(connId, ref)]?.ddl ?? (await catalogDdl(connId, ref));
    const ok = await copyText(ddl);
    logLocal(ok ? 'info' : 'warn', ok ? ls('copiedDdl', { table: tableLabel(ref) }) : ls('copyFailed'));
    return ok;
  } catch (e) {
    logLocal('error', `catalog_ddl: ${errorText(e)}`);
    return false;
  }
}

/**
 * Start an exact count job (`catalog_exact_count`). When the catalog estimate is ≥ 10M rows or
 * unknown it asks first; declining resolves `null`. A small table can finish before the job id
 * arrives (events and the invoke response travel separately): then AppEvents already routed the
 * count and no "counting" flag is set (it would never be cleared).
 */
export async function countExactly(connId: string, ref: TableRef): Promise<string | null> {
  const catalog = useCatalogStore.getState();
  const est = num(catalog.findTable(connId, ref)?.estimatedRows);
  if (est === undefined || est >= COUNT_CONFIRM_ROWS) {
    const ok = await askConfirm({
      title: ls('countConfirmTitle', { name: tableLabel(ref) }),
      body: est === undefined ? ls('countConfirmBodyUnknown') : ls('countConfirmBody', { rows: formatCompact(est) }),
      confirmLabel: ls('countConfirmOk'),
    });
    if (!ok) return null;
  }
  try {
    const jobId = await catalogExactCount(connId, ref);
    useJobsStore.getState().markLocal(jobId);
    const job = useJobsStore.getState().jobs[jobId];
    if (job && isTerminalState(job.summary.state)) return jobId;
    useCatalogStore.getState().setCounting(connId, ref, jobId);
    return jobId;
  } catch (e) {
    logLocal('error', `catalog_exact_count: ${errorText(e)}`);
    return null;
  }
}
