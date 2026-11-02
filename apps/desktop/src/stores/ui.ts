'use client';

// Layout, panel and dialog state (DESIGN.md §7.1 "ui"). A leaf store: it imports no other store.
//
// Panel sizes, open panels, segments, Welcome grouping, collapsed sidebar sections, theme and
// language persist in localStorage (a per-viewer convenience, `persist` version 1; `migrate`
// clamps older values). Everything else is session state.
//
// Persistence is rehydrated explicitly (`skipHydration` + `rehydrateUi()` from the AppShell's
// first effect): the static export prerenders with the defaults, so reading storage during the
// first client render would produce a hydration mismatch.

import type { HostKeyPrompt, SshPasswordPrompt, TableRef } from 'ipc-types';
import { create } from 'zustand';
import { createJSONStorage, persist } from 'zustand/middleware';
import { setLang, type Lang } from '@/lib/i18n';
import { safeLocalStorage } from '@/lib/storage';

export type Theme = 'system' | 'light' | 'dark';
export type SidebarSegment = 'items' | 'history';
export type InspectorSegment = 'activity' | 'details';
export type WelcomeGroupBy = 'none' | 'tag' | 'driver';
export type PalettePick = { kind: 'openInConnection'; tabId: string };
export type PaletteScope = 'all' | 'connections' | 'tables' | 'history' | 'actions';

export interface PaletteState {
  open: boolean;
  scope: PaletteScope;
  query: string;
  pick?: PalettePick;
}

/** A one-shot request to the sidebar (focus the search, open the database / schema popover). */
export interface SidebarRequest {
  kind: 'focusSearch' | 'database' | 'schema';
  nonce: number;
}

export interface ConnectionDialogState {
  open: boolean;
  /** Connection being edited; `null` for a new one. */
  editId: string | null;
}

/** What to do once the password of an "ask every time" connection has been accepted. */
export type DbPasswordReason = 'open' | 'browse' | 'test' | 'run' | 'explain';

export interface DbPasswordRequest {
  reason: DbPasswordReason;
  /** Query tab to run / explain (`run`, `explain`). */
  tabId?: string;
  /** Continuation after the request itself (e.g. `openFromHistory` finishing its work). */
  then?: () => void;
}

/** One password prompt per connection; later requests for the same connection merge into it. */
export interface DbPasswordPrompt {
  id: number;
  connId: string;
  requests: DbPasswordRequest[];
  /** @deprecated Legacy `DbPasswordDialog` only (removed in W3): `requests[0].reason`. */
  reason: DbPasswordReason;
}

/** @deprecated Legacy center tabs (removed in W3). */
export type CenterTab = 'extract' | 'table' | 'history';

/** @deprecated Legacy table selection (removed in W3); the workspace store owns selection. */
export interface SelectedTable {
  connId: string;
  ref: TableRef;
}

/** Size limits (px) and defaults (DESIGN.md §4). */
export const LAYOUT_LIMITS = {
  leftMin: 180,
  leftMax: 480,
  rightMin: 260,
  rightMax: 520,
  logMin: 120,
  logMax: 600,
  exportMin: 120,
  exportMax: 640,
  /** @deprecated Height of the legacy collapsed log bar. */
  logCollapsed: 28,
} as const;

export const LAYOUT_DEFAULTS = {
  leftWidth: 240,
  rightWidth: 300,
  logHeight: 220,
  exportPanelHeight: 280,
} as const;

export interface UiState {
  theme: Theme;
  lang: Lang;
  sidebarOpen: boolean;
  sidebarSegment: SidebarSegment;
  /** Closed by default; persisted once toggled. */
  inspectorOpen: boolean;
  inspectorSegment: InspectorSegment;
  /** Sidebar width. */
  leftWidth: number;
  /** Inspector width. */
  rightWidth: number;
  /** Console drawer height. */
  logHeight: number;
  logOpen: boolean;
  exportPanelHeight: number;
  welcomeGroupBy: WelcomeGroupBy;
  /** Collapsed sidebar sections by key (`items:<kind>`). */
  collapsedSections: Record<string, boolean>;
  sidebarRequest: SidebarRequest | null;
  palette: PaletteState;
  connectionDialog: ConnectionDialogState;
  settingsOpen: boolean;
  aboutOpen: boolean;
  importDbeaverOpen: boolean;
  activityPopoverOpen: boolean;
  /** DB password prompts, oldest first (the dialog shows the head). */
  dbPasswordPrompts: DbPasswordPrompt[];
  /** Tunnel prompts waiting for an answer, oldest first. */
  hostKeyPrompts: HostKeyPrompt[];
  sshPasswordPrompts: SshPasswordPrompt[];
  /** Job whose cancel confirmation is showing (Esc shortcut / cancel button). */
  cancelConfirmJobId: string | null;

  setSidebarOpen(open: boolean): void;
  toggleSidebar(): void;
  setSidebarSegment(s: SidebarSegment): void;
  setInspectorOpen(open: boolean): void;
  toggleInspector(): void;
  setInspectorSegment(s: InspectorSegment): void;
  /** Also opens the sidebar on its Items segment. */
  requestSidebar(kind: SidebarRequest['kind']): void;
  clearSidebarRequest(): void;
  setActivityPopoverOpen(open: boolean): void;
  setLeftWidth(px: number): void;
  setRightWidth(px: number): void;
  setLogHeight(px: number): void;
  setExportPanelHeight(px: number): void;
  setLogOpen(open: boolean): void;
  toggleLog(): void;
  setWelcomeGroupBy(g: WelcomeGroupBy): void;
  toggleSection(key: string): void;
  setTheme(theme: Theme): void;
  setLang(lang: Lang): void;
  openPalette(scope?: PaletteScope, query?: string, pick?: PalettePick): void;
  closePalette(): void;
  openConnectionDialog(editId?: string | null): void;
  closeConnectionDialog(): void;
  setSettingsOpen(open: boolean): void;
  setAboutOpen(open: boolean): void;
  setImportDbeaverOpen(open: boolean): void;
  /**
   * Queue a password request. A connection that already has a queued prompt gets the request
   * appended (nothing is overwritten); otherwise a new prompt joins the queue.
   */
  askDbPassword(connId: string, reason: DbPasswordReason, opts?: { tabId?: string; then?: () => void }): void;
  /** Remove prompt `id` (answered or cancelled). */
  resolveDbPassword(id: number): void;
  pushHostKeyPrompt(p: HostKeyPrompt): void;
  resolveHostKeyPrompt(promptId: string): void;
  pushSshPasswordPrompt(p: SshPasswordPrompt): void;
  resolveSshPasswordPrompt(promptId: string): void;
  setCancelConfirm(jobId: string | null): void;

  // ---- legacy (removed in W3; the old tree / table / extract panels still call these) ----
  /** @deprecated Head of `dbPasswordPrompts`. */
  dbPasswordPrompt: DbPasswordPrompt | null;
  /** @deprecated Resolves the head prompt. */
  closeDbPassword(): void;
  /** @deprecated */
  selectedTable: SelectedTable | null;
  /** @deprecated */
  selectTable(sel: SelectedTable | null): void;
  /** @deprecated */
  centerTab: CenterTab;
  /** @deprecated */
  setCenterTab(tab: CenterTab): void;
}

const clamp = (v: number, min: number, max: number) => Math.max(min, Math.min(max, Math.round(v)));

let nextPromptId = 1;

/**
 * Legacy mirror of the head prompt (removed in W3): enumerable `{ connId, reason }` like the old
 * single prompt, with `id` / `requests` present but non-enumerable.
 */
function legacyHead(p: DbPasswordPrompt | undefined): DbPasswordPrompt | null {
  if (!p) return null;
  const head = { connId: p.connId, reason: p.reason } as DbPasswordPrompt;
  Object.defineProperties(head, { id: { value: p.id }, requests: { value: p.requests } });
  return head;
}

function withPrompts(prompts: DbPasswordPrompt[]): Pick<UiState, 'dbPasswordPrompts' | 'dbPasswordPrompt'> {
  return { dbPasswordPrompts: prompts, dbPasswordPrompt: legacyHead(prompts[0]) };
}

/** The persisted subset. */
type PersistedUi = Pick<
  UiState,
  | 'theme'
  | 'lang'
  | 'sidebarOpen'
  | 'inspectorOpen'
  | 'sidebarSegment'
  | 'inspectorSegment'
  | 'leftWidth'
  | 'rightWidth'
  | 'logHeight'
  | 'exportPanelHeight'
  | 'welcomeGroupBy'
  | 'collapsedSections'
>;

const num = (v: unknown, fallback: number, min: number, max: number) =>
  typeof v === 'number' && Number.isFinite(v) ? clamp(v, min, max) : fallback;

/** Clamp / default a persisted object of any earlier version (exported for tests). */
export function migrateUi(persisted: unknown): Partial<PersistedUi> {
  const p = (persisted && typeof persisted === 'object' ? persisted : {}) as Record<string, unknown>;
  const out: Partial<PersistedUi> = {
    leftWidth: num(p.leftWidth, LAYOUT_DEFAULTS.leftWidth, LAYOUT_LIMITS.leftMin, LAYOUT_LIMITS.leftMax),
    rightWidth: num(p.rightWidth, LAYOUT_DEFAULTS.rightWidth, LAYOUT_LIMITS.rightMin, LAYOUT_LIMITS.rightMax),
    logHeight: num(p.logHeight, LAYOUT_DEFAULTS.logHeight, LAYOUT_LIMITS.logMin, LAYOUT_LIMITS.logMax),
    exportPanelHeight: num(p.exportPanelHeight, LAYOUT_DEFAULTS.exportPanelHeight, LAYOUT_LIMITS.exportMin, LAYOUT_LIMITS.exportMax),
  };
  if (p.theme === 'system' || p.theme === 'light' || p.theme === 'dark') out.theme = p.theme;
  if (p.lang === 'ko' || p.lang === 'en') out.lang = p.lang;
  if (typeof p.sidebarOpen === 'boolean') out.sidebarOpen = p.sidebarOpen;
  if (typeof p.inspectorOpen === 'boolean') out.inspectorOpen = p.inspectorOpen;
  if (p.sidebarSegment === 'items' || p.sidebarSegment === 'history') out.sidebarSegment = p.sidebarSegment;
  if (p.inspectorSegment === 'activity' || p.inspectorSegment === 'details') out.inspectorSegment = p.inspectorSegment;
  if (p.welcomeGroupBy === 'none' || p.welcomeGroupBy === 'tag' || p.welcomeGroupBy === 'driver') out.welcomeGroupBy = p.welcomeGroupBy;
  if (p.collapsedSections && typeof p.collapsedSections === 'object') {
    const cs: Record<string, boolean> = {};
    for (const [k, v] of Object.entries(p.collapsedSections as Record<string, unknown>)) if (v === true) cs[k] = true;
    out.collapsedSections = cs;
  }
  return out;
}

const CLOSED_PALETTE: PaletteState = { open: false, scope: 'all', query: '' };

export const useUiStore = create<UiState>()(
  persist(
    (set, get) => ({
      theme: 'system',
      lang: 'ko',
      sidebarOpen: true,
      sidebarSegment: 'items',
      inspectorOpen: false,
      inspectorSegment: 'activity',
      leftWidth: LAYOUT_DEFAULTS.leftWidth,
      rightWidth: LAYOUT_DEFAULTS.rightWidth,
      logHeight: LAYOUT_DEFAULTS.logHeight,
      logOpen: false,
      exportPanelHeight: LAYOUT_DEFAULTS.exportPanelHeight,
      welcomeGroupBy: 'none',
      collapsedSections: {},
      sidebarRequest: null,
      palette: CLOSED_PALETTE,
      connectionDialog: { open: false, editId: null },
      settingsOpen: false,
      aboutOpen: false,
      importDbeaverOpen: false,
      activityPopoverOpen: false,
      dbPasswordPrompts: [],
      dbPasswordPrompt: null,
      hostKeyPrompts: [],
      sshPasswordPrompts: [],
      cancelConfirmJobId: null,
      selectedTable: null,
      centerTab: 'extract',

      setSidebarOpen: (open) => set({ sidebarOpen: open }),
      toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
      setSidebarSegment: (sidebarSegment) => set({ sidebarSegment }),
      setInspectorOpen: (open) => set({ inspectorOpen: open }),
      toggleInspector: () => set((s) => ({ inspectorOpen: !s.inspectorOpen })),
      setInspectorSegment: (inspectorSegment) => set({ inspectorSegment }),
      requestSidebar: (kind) =>
        set((s) => ({
          sidebarRequest: { kind, nonce: (s.sidebarRequest?.nonce ?? 0) + 1 },
          sidebarOpen: true,
          sidebarSegment: 'items',
        })),
      clearSidebarRequest: () => set({ sidebarRequest: null }),
      setActivityPopoverOpen: (activityPopoverOpen) => set({ activityPopoverOpen }),
      setLeftWidth: (px) => set({ leftWidth: clamp(px, LAYOUT_LIMITS.leftMin, LAYOUT_LIMITS.leftMax) }),
      setRightWidth: (px) => set({ rightWidth: clamp(px, LAYOUT_LIMITS.rightMin, LAYOUT_LIMITS.rightMax) }),
      setLogHeight: (px) => set({ logHeight: clamp(px, LAYOUT_LIMITS.logMin, LAYOUT_LIMITS.logMax) }),
      setExportPanelHeight: (px) =>
        set({ exportPanelHeight: clamp(px, LAYOUT_LIMITS.exportMin, LAYOUT_LIMITS.exportMax) }),
      setLogOpen: (open) => set({ logOpen: open }),
      toggleLog: () => set((s) => ({ logOpen: !s.logOpen })),
      setWelcomeGroupBy: (welcomeGroupBy) => set({ welcomeGroupBy }),
      toggleSection: (key) =>
        set((s) => {
          const collapsedSections = { ...s.collapsedSections };
          if (collapsedSections[key]) delete collapsedSections[key];
          else collapsedSections[key] = true;
          return { collapsedSections };
        }),
      setTheme: (theme) => set({ theme }),
      setLang: (lang) => {
        setLang(lang);
        set({ lang });
      },
      openPalette: (scope = 'all', query = '', pick) => set({ palette: { open: true, scope, query, pick } }),
      closePalette: () => set({ palette: CLOSED_PALETTE }),
      openConnectionDialog: (editId = null) => set({ connectionDialog: { open: true, editId } }),
      closeConnectionDialog: () => set({ connectionDialog: { open: false, editId: null } }),
      setSettingsOpen: (open) => set({ settingsOpen: open }),
      setAboutOpen: (aboutOpen) => set({ aboutOpen }),
      setImportDbeaverOpen: (importDbeaverOpen) => set({ importDbeaverOpen }),
      askDbPassword: (connId, reason, opts) => {
        const request: DbPasswordRequest = { reason, tabId: opts?.tabId, then: opts?.then };
        const prompts = get().dbPasswordPrompts;
        const existing = prompts.find((p) => p.connId === connId);
        if (existing) {
          set(withPrompts(prompts.map((p) => (p === existing ? { ...p, requests: [...p.requests, request] } : p))));
          return;
        }
        set(withPrompts([...prompts, { id: nextPromptId++, connId, requests: [request], reason }]));
      },
      resolveDbPassword: (id) => set((s) => withPrompts(s.dbPasswordPrompts.filter((p) => p.id !== id))),
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

      closeDbPassword: () => set((s) => withPrompts(s.dbPasswordPrompts.slice(1))),
      selectTable: (sel) => set((s) => ({ selectedTable: sel, centerTab: sel ? 'table' : s.centerTab })),
      setCenterTab: (tab) => set({ centerTab: tab }),
    }),
    {
      name: 'bulkdb-ui',
      version: 1,
      // Never the raw `localStorage`: it can be undefined or throw (see `lib/storage`).
      storage: createJSONStorage(() => safeLocalStorage()),
      skipHydration: true,
      partialize: (s): PersistedUi => ({
        theme: s.theme,
        lang: s.lang,
        sidebarOpen: s.sidebarOpen,
        inspectorOpen: s.inspectorOpen,
        sidebarSegment: s.sidebarSegment,
        inspectorSegment: s.inspectorSegment,
        leftWidth: s.leftWidth,
        rightWidth: s.rightWidth,
        logHeight: s.logHeight,
        exportPanelHeight: s.exportPanelHeight,
        welcomeGroupBy: s.welcomeGroupBy,
        collapsedSections: s.collapsedSections,
      }),
      migrate: (persisted) => migrateUi(persisted) as UiState,
      // Also clamp current-version values (hand-edited storage, changed limits).
      merge: (persisted, current) => ({ ...current, ...migrateUi(persisted) }),
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
