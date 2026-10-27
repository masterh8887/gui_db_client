'use client';

// Saved connections, driver availability and per-connection test status.

import type { Connection, ConnectionInput, DriverInfo, ImportedConnection, ProbeResult } from 'ipc-types';
import { create } from 'zustand';
import {
  connDelete,
  connGet,
  connImportDbeaver,
  connList,
  connSave,
  connTest,
  driversList,
  driversRescan,
} from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';

export type ConnStatus = 'unknown' | 'testing' | 'ok' | 'error';

export interface ConnectionsState {
  connections: Connection[];
  drivers: DriverInfo[];
  selectedId: string | null;
  status: Record<string, ConnStatus>;
  /**
   * "Ask every time" connections whose password was accepted this session (the backend keeps
   * that password in memory until the app quits; the UI never stores it).
   */
  unlocked: Record<string, boolean>;
  lastProbe: Record<string, ProbeResult>;
  errors: Record<string, string>;
  loading: boolean;
  loaded: boolean;

  load(): Promise<void>;
  loadDrivers(): Promise<void>;
  rescanDrivers(): Promise<void>;
  setDrivers(drivers: DriverInfo[]): void;
  select(id: string | null): void;
  /**
   * Re-read one connection (`conn_get`) and replace it in the list, so `hasPassword` /
   * `hasSshSecret` reflect the keyring right now (e.g. when its edit dialog opens).
   */
  refreshConnection(id: string): Promise<Connection>;
  /**
   * Save through `conn_save`. `clearPassword` also forgets that the password was given this
   * session (the backend drops it), so an "ask every time" connection asks again.
   */
  save(input: ConnectionInput): Promise<Connection>;
  remove(id: string): Promise<void>;
  test(id: string, password?: string): Promise<ProbeResult>;
  /** Parse a DBeaver `data-sources.json` (nothing is saved yet). */
  importDbeaver(path: string): Promise<ImportedConnection[]>;
  /**
   * Save rows returned by `importDbeaver` as new connections (without passwords: DBeaver's are
   * never imported), one by one so a bad row does not stop the others, then reload `conn_list`.
   */
  saveImported(rows: ImportedConnection[]): Promise<ImportSaveResult>;
}

/** Outcome of `saveImported`. */
export interface ImportSaveResult {
  saved: Connection[];
  /** `index` is the position of the row in the `rows` argument. */
  failed: { index: number; name: string; error: string }[];
}

/**
 * How an imported DBeaver row relates to this machine: `unsupported` when BulkDB has no such
 * driver (saving would be rejected), `driverMissing` when the driver exists but its vendor client
 * is not installed (it can be saved and used after installing it and rescanning), `exists` when a
 * connection with the same name is already saved, `ok` otherwise.
 */
export type ImportRowStatus = 'ok' | 'unsupported' | 'driverMissing' | 'exists';

export function importRowStatus(row: ImportedConnection, drivers: DriverInfo[], connections: Connection[]): ImportRowStatus {
  const driver = drivers.find((d) => d.id === row.connection.driver);
  if (!driver) return 'unsupported';
  const name = row.connection.name.trim().toLowerCase();
  if (connections.some((c) => c.name.trim().toLowerCase() === name)) return 'exists';
  return driver.available ? 'ok' : 'driverMissing';
}

export const useConnectionsStore = create<ConnectionsState>()((set, get) => ({
  connections: [],
  drivers: [],
  selectedId: null,
  status: {},
  unlocked: {},
  lastProbe: {},
  errors: {},
  loading: false,
  loaded: false,

  load: async () => {
    set({ loading: true });
    try {
      const [connections, drivers] = await Promise.all([connList(), driversList()]);
      set((s) => ({
        connections,
        drivers,
        loaded: true,
        selectedId: s.selectedId && connections.some((c) => c.id === s.selectedId) ? s.selectedId : (connections[0]?.id ?? null),
      }));
    } finally {
      set({ loading: false });
    }
  },

  loadDrivers: async () => {
    set({ drivers: await driversList() });
  },

  rescanDrivers: async () => {
    set({ drivers: await driversRescan() });
  },

  setDrivers: (drivers) => set({ drivers }),

  select: (id) => set({ selectedId: id }),

  refreshConnection: async (id) => {
    const fresh = await connGet(id);
    set((s) => ({ connections: s.connections.map((c) => (c.id === fresh.id ? fresh : c)) }));
    return fresh;
  },

  save: async (input) => {
    const saved = await connSave(input);
    set((s) => {
      const exists = s.connections.some((c) => c.id === saved.id);
      const unlocked = { ...s.unlocked };
      if (input.clearPassword) delete unlocked[saved.id];
      return {
        connections: exists ? s.connections.map((c) => (c.id === saved.id ? saved : c)) : [...s.connections, saved],
        selectedId: s.selectedId ?? saved.id,
        // Editing a connection invalidates its last test.
        status: { ...s.status, [saved.id]: 'unknown' },
        unlocked,
      };
    });
    return saved;
  },

  remove: async (id) => {
    await connDelete(id);
    set((s) => {
      const status = { ...s.status };
      const unlocked = { ...s.unlocked };
      const lastProbe = { ...s.lastProbe };
      const errors = { ...s.errors };
      delete status[id];
      delete unlocked[id];
      delete lastProbe[id];
      delete errors[id];
      const connections = s.connections.filter((c) => c.id !== id);
      return {
        connections,
        status,
        unlocked,
        lastProbe,
        errors,
        selectedId: s.selectedId === id ? (connections[0]?.id ?? null) : s.selectedId,
      };
    });
  },

  test: async (id, password) => {
    set((s) => ({ status: { ...s.status, [id]: 'testing' } }));
    try {
      const probe = await connTest(id, password);
      set((s) => {
        const errors = { ...s.errors };
        delete errors[id];
        return {
          status: { ...s.status, [id]: 'ok' },
          unlocked: password ? { ...s.unlocked, [id]: true } : s.unlocked,
          lastProbe: { ...s.lastProbe, [id]: probe },
          errors,
        };
      });
      return probe;
    } catch (e) {
      set((s) => ({ status: { ...s.status, [id]: 'error' }, errors: { ...s.errors, [id]: errorText(e) } }));
      throw e;
    }
  },

  importDbeaver: async (path) => connImportDbeaver(path),

  saveImported: async (rows) => {
    const result: ImportSaveResult = { saved: [], failed: [] };
    for (const [index, row] of rows.entries()) {
      try {
        // An empty id makes the backend create a new connection (like `conn import-dbeaver --save`).
        result.saved.push(await connSave({ ...row.connection, id: '' }));
      } catch (e) {
        result.failed.push({ index, name: row.connection.name, error: errorText(e) });
      }
    }
    if (result.saved.length > 0) await get().load();
    return result;
  },
}));

export function selectSelectedConnection(s: ConnectionsState): Connection | undefined {
  return s.connections.find((c) => c.id === s.selectedId);
}

export function selectDriver(s: ConnectionsState, driverId: string | undefined): DriverInfo | undefined {
  return s.drivers.find((d) => d.id === driverId);
}

/**
 * Whether `connId` is a password-based connection saved with "ask every time" whose password
 * has not been given yet this session. Callers ask for it (ui `askDbPassword`) before any call
 * that opens a database session (catalog, EXPLAIN, export).
 */
export function needsSessionPassword(s: ConnectionsState, connId: string | null | undefined): boolean {
  const conn = s.connections.find((c) => c.id === connId);
  if (!conn || conn.savePassword) return false;
  if (!selectDriver(s, conn.driver)?.fields.includes('password')) return false;
  return !s.unlocked[conn.id];
}

