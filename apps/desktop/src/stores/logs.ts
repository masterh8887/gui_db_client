'use client';

// Ring buffer of `LogEvent`s for the bottom panel. An error entry flags the panel to auto-expand.

import type { LogEvent, LogLevel } from 'ipc-types';
import { create } from 'zustand';
import { logsRecent } from '@/lib/commands';
import { useUiStore } from './ui';

export const LOG_RING_SIZE = 2000;

export const LOG_LEVEL_ORDER: Record<LogLevel, number> = { trace: 0, debug: 1, info: 2, warn: 3, error: 4 };

export interface LogsState {
  entries: LogEvent[];
  /** Set when an error arrived since the user last looked; cleared by `ackError`. */
  errorFlag: boolean;
  errorCount: number;
  lastErrorAt: string | null;
  /** Minimum level shown in the panel. */
  filterLevel: LogLevel;
  push(ev: LogEvent): void;
  pushMany(evs: LogEvent[]): void;
  clear(): void;
  ackError(): void;
  setFilter(level: LogLevel): void;
  loadRecent(limit?: number): Promise<void>;
}

function trim(entries: LogEvent[]): LogEvent[] {
  return entries.length > LOG_RING_SIZE ? entries.slice(entries.length - LOG_RING_SIZE) : entries;
}

export const useLogsStore = create<LogsState>()((set) => ({
  entries: [],
  errorFlag: false,
  errorCount: 0,
  lastErrorAt: null,
  filterLevel: 'info',

  push: (ev) => {
    set((s) => ({
      entries: trim([...s.entries, ev]),
      ...(ev.level === 'error'
        ? { errorFlag: true, errorCount: s.errorCount + 1, lastErrorAt: ev.at }
        : {}),
    }));
    if (ev.level === 'error') useUiStore.getState().setLogOpen(true);
  },
  pushMany: (evs) => {
    if (evs.length === 0) return;
    const errors = evs.filter((e) => e.level === 'error');
    set((s) => ({
      entries: trim([...s.entries, ...evs]),
      ...(errors.length > 0
        ? { errorFlag: true, errorCount: s.errorCount + errors.length, lastErrorAt: errors[errors.length - 1].at }
        : {}),
    }));
    if (errors.length > 0) useUiStore.getState().setLogOpen(true);
  },
  clear: () => set({ entries: [], errorFlag: false, errorCount: 0, lastErrorAt: null }),
  ackError: () => set({ errorFlag: false }),
  setFilter: (level) => set({ filterLevel: level }),
  loadRecent: async (limit = 500) => {
    const recent = await logsRecent(limit);
    // Replace, not append: `logs_recent` is the authoritative backlog at startup.
    set({ entries: trim(recent) });
  },
}));

/** Log something that happened in the UI itself (failed IPC call, user action) into the panel. */
export function logLocal(level: LogLevel, message: string, jobId?: string): void {
  useLogsStore.getState().push({ level, message, at: new Date().toISOString(), target: 'ui', jobId });
}

/** Entries at or above the current filter level. */
export function selectVisibleLogs(s: LogsState): LogEvent[] {
  const min = LOG_LEVEL_ORDER[s.filterLevel];
  return s.entries.filter((e) => LOG_LEVEL_ORDER[e.level] >= min);
}
