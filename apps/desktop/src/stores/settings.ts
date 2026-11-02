'use client';

import type { Settings } from 'ipc-types';
import { create } from 'zustand';
import { settingsGet, settingsSet } from '@/lib/commands';

export interface SettingsState {
  settings: Settings | null;
  loading: boolean;
  saving: boolean;
  load(): Promise<Settings>;
  /**
   * Resolves once settings are loaded: immediately when they already are, otherwise after the
   * load in flight (or a new one). Resolves on failure too, so callers never block on it.
   */
  ensureLoaded(): Promise<void>;
  save(patch: Partial<Settings>): Promise<Settings>;
}

/** The `settings_get` in flight, shared by `load` and `ensureLoaded`. */
let inflight: Promise<Settings> | null = null;

/** Tests: forget a load in flight (a mock reset leaves it pending forever). */
export function __resetSettingsForTests(): void {
  inflight = null;
}

export const useSettingsStore = create<SettingsState>()((set, get) => ({
  settings: null,
  loading: false,
  saving: false,

  load: () => {
    inflight ??= (async () => {
      set({ loading: true });
      try {
        const settings = await settingsGet();
        set({ settings });
        return settings;
      } finally {
        set({ loading: false });
        inflight = null;
      }
    })();
    return inflight;
  },

  ensureLoaded: async () => {
    if (get().settings) return;
    try {
      await get().load();
    } catch {
      // Defaults apply; the bootstrap logs the failure.
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
