'use client';

// Selection / layout / dialog state. Panel sizes, theme and language persist in localStorage (a
// per-viewer convenience); everything else is session state.
//
// Persistence is rehydrated explicitly (`skipHydration` + `rehydrateUi()` from the AppShell's
// first effect): the static export prerenders with the defaults, so reading storage during the
// first client render would produce a hydration mismatch.

import type { HostKeyPrompt, SshPasswordPrompt, TableRef } from 'ipc-types';
import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';
import { setLang, type Lang } from '@/lib/i18n';
import { safeLocalStorage } from '@/lib/storage';

export type CenterTab = 'extract' | 'table' | 'history';
export type Theme = 'system' | 'light' | 'dark';

export interface SelectedTable {
  connId: string;
  ref: TableRef;
}

export interface ConnectionDialogState {
  open: boolean;
  /** Connection being edited; `null` for a new one. */
  editId: string | null;
}

/** What to do once the password of an "ask every time" connection has been accepted. */
export type DbPasswordReason = 'browse' | 'test' | 'run' | 'explain';

export interface DbPasswordPrompt {
  connId: string;
  reason: DbPasswordReason;
}

export const LAYOUT_LIMITS = {
  leftMin: 200,
  leftMax: 600,
  rightMin: 240,
  rightMax: 640,
  logMin: 96,
  logMax: 600,
  /** Height of the collapsed log bar (one row). */
  logCollapsed: 28,
} as const;

export interface UiState {
  selectedTable: SelectedTable | null;
  centerTab: CenterTab;
  leftWidth: number;
  rightWidth: number;
  logHeight: number;
  logOpen: boolean;
  theme: Theme;
  lang: Lang;
  connectionDialog: ConnectionDialogState;
  settingsOpen: boolean;
  /** Password request for a connection saved without its password (`savePassword = false`). */
  dbPasswordPrompt: DbPasswordPrompt | null;
  /** Tunnel prompts waiting for an answer, oldest first. */
  hostKeyPrompts: HostKeyPrompt[];
  sshPasswordPrompts: SshPasswordPrompt[];
  /** Job whose cancel confirmation is showing (Esc shortcut / cancel button). */
  cancelConfirmJobId: string | null;

  selectTable(sel: SelectedTable | null): void;
  setCenterTab(tab: CenterTab): void;
  setLeftWidth(px: number): void;
  setRightWidth(px: number): void;
  setLogHeight(px: number): void;
  setLogOpen(open: boolean): void;
  toggleLog(): void;
  setTheme(theme: Theme): void;
  setLang(lang: Lang): void;
  openConnectionDialog(editId?: string | null): void;
  closeConnectionDialog(): void;
  setSettingsOpen(open: boolean): void;
  askDbPassword(connId: string, reason: DbPasswordReason): void;
  closeDbPassword(): void;
  pushHostKeyPrompt(p: HostKeyPrompt): void;
  resolveHostKeyPrompt(promptId: string): void;
  pushSshPasswordPrompt(p: SshPasswordPrompt): void;
  resolveSshPasswordPrompt(promptId: string): void;
  setCancelConfirm(jobId: string | null): void;
}

const clamp = (v: number, min: number, max: number) => Math.max(min, Math.min(max, Math.round(v)));

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      selectedTable: null,
      centerTab: 'extract',
      leftWidth: 280,
      rightWidth: 320,
      logHeight: 180,
      logOpen: false,
      theme: 'system',
      lang: 'ko',
      connectionDialog: { open: false, editId: null },
      settingsOpen: false,
      dbPasswordPrompt: null,
      hostKeyPrompts: [],
      sshPasswordPrompts: [],
      cancelConfirmJobId: null,

      selectTable: (sel) => set((s) => ({ selectedTable: sel, centerTab: sel ? 'table' : s.centerTab })),
      setCenterTab: (tab) => set({ centerTab: tab }),
      setLeftWidth: (px) => set({ leftWidth: clamp(px, LAYOUT_LIMITS.leftMin, LAYOUT_LIMITS.leftMax) }),
      setRightWidth: (px) => set({ rightWidth: clamp(px, LAYOUT_LIMITS.rightMin, LAYOUT_LIMITS.rightMax) }),
      setLogHeight: (px) => set({ logHeight: clamp(px, LAYOUT_LIMITS.logMin, LAYOUT_LIMITS.logMax) }),
      setLogOpen: (open) => set({ logOpen: open }),
      toggleLog: () => set((s) => ({ logOpen: !s.logOpen })),
      setTheme: (theme) => set({ theme }),
      setLang: (lang) => {
        setLang(lang);
        set({ lang });
      },
      openConnectionDialog: (editId = null) => set({ connectionDialog: { open: true, editId } }),
      closeConnectionDialog: () => set({ connectionDialog: { open: false, editId: null } }),
      setSettingsOpen: (open) => set({ settingsOpen: open }),
      askDbPassword: (connId, reason) => set({ dbPasswordPrompt: { connId, reason } }),
      closeDbPassword: () => set({ dbPasswordPrompt: null }),
      pushHostKeyPrompt: (p) =>
        set((s) => (s.hostKeyPrompts.some((x) => x.promptId === p.promptId) ? s : { hostKeyPrompts: [...s.hostKeyPrompts, p] })),
      resolveHostKeyPrompt: (id) => set((s) => ({ hostKeyPrompts: s.hostKeyPrompts.filter((p) => p.promptId !== id) })),
      pushSshPasswordPrompt: (p) =>
        set((s) =>
          s.sshPasswordPrompts.some((x) => x.promptId === p.promptId) ? s : { sshPasswordPrompts: [...s.sshPasswordPrompts, p] },
        ),
      resolveSshPasswordPrompt: (id) =>
        set((s) => ({ sshPasswordPrompts: s.sshPasswordPrompts.filter((p) => p.promptId !== id) })),
      setCancelConfirm: (jobId) => set({ cancelConfirmJobId: jobId }),
    }),
    {
      name: 'bulkdb-ui',
      // Never the raw `localStorage`: it can be undefined or throw (see `lib/storage`).
      storage: createJSONStorage(() => safeLocalStorage()),
      skipHydration: true,
      partialize: (s) => ({
        leftWidth: s.leftWidth,
        rightWidth: s.rightWidth,
        logHeight: s.logHeight,
        theme: s.theme,
        lang: s.lang,
      }),
      onRehydrateStorage: () => (state) => {
        if (state) setLang(state.lang);
      },
    },
  ),
);

/** Load the persisted panel sizes / theme / language. Call once after the first client render. */
export function rehydrateUi(): Promise<void> {
  return Promise.resolve(useUiStore.persist.rehydrate());
}

/** Apply the theme to `<html data-theme>`; `system` removes the attribute (CSS media query rules). */
export function applyTheme(theme: Theme): void {
  if (typeof document === 'undefined') return;
  const el = document.documentElement;
  if (theme === 'system') delete el.dataset.theme;
  else el.dataset.theme = theme;
}
