'use client';

// Export history pages, filtered server side (`history_list(connId?, limit)`). One page per
// filter key: a connection id, or `'*'` for every connection. Client-side search covers the
// loaded page only.

import type { HistoryEntry } from 'ipc-types';
import { create } from 'zustand';
import { historyList } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';

export interface HistoryPage {
  entries: HistoryEntry[];
  loading: boolean;
  error: string | null;
}

export const HISTORY_ALL = '*';
export const HISTORY_LIMIT = 500;

/** Page key for a filter: the connection id, or `'*'` for all connections. */
export function historyKey(connId: string | null): string {
  return connId ?? HISTORY_ALL;
}

const EMPTY_PAGE: HistoryPage = { entries: [], loading: false, error: null };

export interface HistoryState {
  lists: Record<string, HistoryPage>;
  /**
   * Load the page for `connId` (`null` = every connection) through `history_list`.
   * Called without arguments it refreshes the legacy flat page (removed in W3).
   */
  load(connId?: string | null, limit?: number): Promise<void>;
  /** Reload every page loaded so far (AppEvents calls it after an export). */
  reloadLoaded(): Promise<void>;

  /** @deprecated Legacy flat page (HistoryTab), removed in W3. */
  entries: HistoryEntry[];
  /** @deprecated */
  loading: boolean;
  /** @deprecated */
  error: string | null;
  /** @deprecated Filter of the flat page: `null` means every connection. */
  connId: string | null;
  /** @deprecated */
  setConnId(connId: string | null): Promise<void>;
}

/** The page for a key (an empty one when not loaded yet). */
export function selectHistoryPage(s: HistoryState, connId: string | null): HistoryPage {
  return s.lists[historyKey(connId)] ?? EMPTY_PAGE;
}

export const useHistoryStore = create<HistoryState>()((set, get) => {
  const setPage = (key: string, patch: Partial<HistoryPage>) =>
    set((s) => ({ lists: { ...s.lists, [key]: { ...(s.lists[key] ?? EMPTY_PAGE), ...patch } } }));

  const loadPage = async (connId: string | null, limit: number, flat: boolean) => {
    const key = historyKey(connId);
    setPage(key, { loading: true, error: null });
    if (flat) set({ loading: true, error: null });
    try {
      const entries = await historyList(connId ?? undefined, limit);
      setPage(key, { entries });
      if (flat && get().connId === connId) set({ entries });
    } catch (e) {
      setPage(key, { error: errorText(e) });
      if (flat) set({ error: errorText(e) });
    } finally {
      setPage(key, { loading: false });
      if (flat) set({ loading: false });
    }
  };

  return {
    lists: {},
    entries: [],
    loading: false,
    error: null,
    connId: null,

    load: async (connId, limit = HISTORY_LIMIT) => {
      if (connId === undefined) return loadPage(get().connId, limit, true);
      return loadPage(connId, limit, false);
    },

    reloadLoaded: async () => {
      const keys = Object.keys(get().lists);
      await Promise.all(
        keys.map((k) => {
          const id = k === HISTORY_ALL ? null : k;
          return id === get().connId ? get().load() : get().load(id);
        }),
      );
    },

    setConnId: async (connId) => {
      set({ connId });
      await get().load();
    },
  };
});
