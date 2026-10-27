'use client';

import type { HistoryEntry } from 'ipc-types';
import { create } from 'zustand';
import { historyList } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';

export interface HistoryState {
  entries: HistoryEntry[];
  loading: boolean;
  error: string | null;
  /** Filter: `null` means every connection. */
  connId: string | null;
  load(connId?: string | null, limit?: number): Promise<void>;
  setConnId(connId: string | null): Promise<void>;
}

export const useHistoryStore = create<HistoryState>()((set, get) => ({
  entries: [],
  loading: false,
  error: null,
  connId: null,

  load: async (connId = get().connId, limit = 200) => {
    set({ loading: true, error: null });
    try {
      const entries = await historyList(connId ?? undefined, limit);
      set({ entries });
    } catch (e) {
      set({ error: errorText(e) });
    } finally {
      set({ loading: false });
    }
  },

  setConnId: async (connId) => {
    set({ connId });
    await get().load(connId);
  },
}));
