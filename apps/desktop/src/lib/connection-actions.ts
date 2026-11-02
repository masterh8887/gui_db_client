'use client';

// Connection actions shared by Welcome, the toolbar switcher, the status pill popover and the
// palette (DESIGN.md §7.5).

import type { Connection, ConnectionInput, ProbeResult } from 'ipc-types';
import { useCatalogStore } from '@/stores/catalog';
import { needsSessionPassword, selectDriver, useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { logLocal } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { askConfirm } from './confirm';
import { errorText } from './ipc-error';
import { ls } from './strings';
import { toast } from './toast';

export function newConnection(): void {
  useUiStore.getState().openConnectionDialog(null);
}

export function editConnection(connId: string): void {
  useUiStore.getState().openConnectionDialog(connId);
}

/** Test a saved connection; asks for an "ask every time" password first (reason `test`). */
export async function testConnection(connId: string): Promise<ProbeResult | null> {
  if (needsSessionPassword(useConnectionsStore.getState(), connId)) {
    useUiStore.getState().askDbPassword(connId, 'test');
    return null;
  }
  try {
    return await useConnectionsStore.getState().test(connId);
  } catch {
    // The status (error + message) is in the connections store.
    return null;
  }
}

/** Confirm, delete, forget its catalog and close its workspace (no second confirm). */
export async function deleteConnection(connId: string): Promise<boolean> {
  const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId);
  if (!conn) return false;
  const ok = await askConfirm({
    title: ls('deleteConfirmTitle', { name: conn.name }),
    body: ls('deleteConfirmBody'),
    confirmLabel: ls('deleteConfirmOk'),
    danger: true,
  });
  if (!ok) return false;
  try {
    await useConnectionsStore.getState().remove(connId);
  } catch (e) {
    const error = errorText(e);
    logLocal('error', ls('deleteFailed', { error }));
    toast(ls('deleteFailed', { error }), { tone: 'danger' });
    return false;
  }
  useCatalogStore.getState().clearConnection(connId);
  await useWorkspaceStore.getState().closeWorkspace(connId, { force: true });
  logLocal('info', `Connection deleted: ${conn.name}`);
  return true;
}

/**
 * Stop allowing DB copy to write into `conn` (`conn_save` of the stored connection with
 * `writeAllowed: false`; db-copy.md 2) and reload the list. Rejects with the save error.
 */
export async function clearWriteAllowed(conn: Connection): Promise<void> {
  const cs = useConnectionsStore.getState();
  // Re-read the stored connection first (`conn_get`): the Welcome-list copy can be up to one
  // config_poll stale, and spec 7 saves the stored connection (the copy store does the same).
  const stored = await cs.refreshConnection(conn.id);
  const { hasPassword: _hp, hasSshSecret: _hs, ...rest } = stored;
  void _hp;
  void _hs;
  await cs.save({ ...rest, hasPassword: false, hasSshSecret: false, writeAllowed: false } as ConnectionInput);
  await useConnectionsStore.getState().load();
  logLocal('info', `Write access cleared: ${conn.name}`);
}

/**
 * Save a copy through `conn_save` (`id: ''`): name "<name> 복사본", no secrets, the output-only
 * `hasPassword` / `hasSshSecret` stripped, and "ask every time" when the driver uses a password,
 * so the first use asks through the password prompt instead of failing with AUTH.
 */
export async function duplicateConnection(connId: string): Promise<Connection | null> {
  const cs = useConnectionsStore.getState();
  const conn = cs.connections.find((c) => c.id === connId);
  if (!conn) return null;
  const usesPassword = !!selectDriver(cs, conn.driver)?.fields.includes('password');
  const copy: Record<string, unknown> = {
    ...conn,
    id: '',
    name: ls('copyName', { name: conn.name }),
    savePassword: usesPassword ? false : conn.savePassword,
  };
  delete copy.hasPassword;
  delete copy.hasSshSecret;
  // A duplicate never starts write-allowed (core ignores it on a new id anyway; db-copy.md 2).
  delete copy.writeAllowed;
  try {
    const saved = await cs.save(copy as unknown as ConnectionInput);
    toast(ls('duplicated', { name: conn.name }));
    logLocal('info', `Connection duplicated: ${conn.name} -> ${saved.name}`);
    return saved;
  } catch (e) {
    const error = errorText(e);
    logLocal('error', ls('duplicateFailed', { error }));
    toast(ls('duplicateFailed', { error }), { tone: 'danger' });
    return null;
  }
}

/**
 * After an existing connection was saved: when its workspace is in memory, reload its metadata
 * (`catalog.refresh`), resolve the browse scope again and re-test it (refreshes the pill). Docs
 * keep their SQL.
 */
export async function afterConnectionSaved(connId: string): Promise<void> {
  const ws = useWorkspaceStore.getState();
  if (!ws.opened.includes(connId)) return;
  toast(ls('metadataReload'));
  useWorkspaceStore.setState((s) => {
    const scope = { ...s.scope };
    delete scope[connId];
    return { scope };
  });
  try {
    await useCatalogStore.getState().refresh(connId);
  } catch (e) {
    logLocal('error', `catalog_invalidate: ${errorText(e)}`);
  }
  await useWorkspaceStore.getState().ensureScope(connId).catch(() => undefined);
  if (!needsSessionPassword(useConnectionsStore.getState(), connId)) {
    await useConnectionsStore.getState().test(connId).catch(() => undefined);
  }
}

/**
 * Re-target a query doc: copy `{ sql, sqlMode, sqlFile, name, output }` into a new query tab of
 * `targetConnId`'s workspace (the session comes from the target's defaults).
 */
export async function openQueryInConnection(tabId: string, targetConnId: string): Promise<string | null> {
  const doc = useExtractStore.getState().docs[tabId];
  if (!doc) return null;
  const init = { sql: doc.sql, sqlMode: doc.sqlMode, sqlFile: doc.sqlFile, name: doc.name, output: { ...doc.output } };
  if (needsSessionPassword(useConnectionsStore.getState(), targetConnId)) {
    useUiStore.getState().askDbPassword(targetConnId, 'open', {
      then: () => void useTabsStore.getState().openQuery(targetConnId, init),
    });
    return null;
  }
  const ok = await useWorkspaceStore.getState().openWorkspace(targetConnId, { skipEmptyTab: true });
  if (!ok) return null;
  return useTabsStore.getState().openQuery(targetConnId, init);
}

/** Rescan vendor clients; logs and toasts "N개 사용 가능". */
export async function rescanDrivers(): Promise<number | null> {
  try {
    await useConnectionsStore.getState().rescanDrivers();
    const count = useConnectionsStore.getState().drivers.filter((d) => d.available).length;
    logLocal('info', ls('rescanned', { count }));
    toast(ls('rescanned', { count }), { tone: 'success' });
    return count;
  } catch (e) {
    const error = errorText(e);
    logLocal('error', ls('rescanFailed', { error }));
    toast(ls('rescanFailed', { error }), { tone: 'danger' });
    return null;
  }
}

export function startImportDbeaver(): void {
  useUiStore.getState().setImportDbeaverOpen(true);
}
