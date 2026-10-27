'use client';

import type { Settings } from 'ipc-types';
import { create } from 'zustand';
import { settingsGet, settingsSet } from '@/lib/commands';

export interface SettingsState {
  settings: Settings | null;
  loading: boolean;
  saving: boolean;
  load(): Promise<Settings>;
  save(patch: Partial<Settings>): Promise<Settings>;
}

export const useSettingsStore = create<SettingsState>()((set, get) => ({
  settings: null,
  loading: false,
  saving: false,

  load: async () => {
    set({ loading: true });
    try {
      const settings = await settingsGet();
      set({ settings });
      return settings;
    } finally {
      set({ loading: false });
    }
  },

  save: async (patch) => {
    const current = get().settings ?? (await get().load());
    set({ saving: true });
    try {
      const saved = await settingsSet({ ...current, ...patch });
      set({ settings: saved });
      return saved;
    } finally {
      set({ saving: false });
    }
  },
}));
