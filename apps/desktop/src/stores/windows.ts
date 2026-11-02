'use client';

// Several windows of one process and several BulkDB processes (docs/db-copy.md 1). The window
// registry of record lives in the Tauri shell (`windows:changed`); this store mirrors it for the
// connection manager (window chips), the toolbar and the status pill, and owns this window's
// side of the protocol:
// - `initWindows()` (AppShell, once): `window_init`, the window list, the workspace → title
//   subscription (`window_set_connection`), the shared ui slice (`storage` event → rehydrate),
//   the close question (`onCloseRequested`), the quit question (`app:quit-requested`) and the
//   other-instances poll; then `window_ready` (this page listens: the shell routes tunnel
//   prompts, job hand-overs and the quit question here and re-sends what it missed while
//   loading) and opens the connection the window was created for.
// - Closing: whether this is the last window is the shell's answer (`window_close_begin`,
//   decided under its registry lock), never this page's copy of `windows:changed`, which is
//   stale when several windows close at once.
// - Per-process facts (jobs, the job limit, SSH tunnels, typed passwords) are explained by the
//   instances chip (Welcome header, status pill popover).

import type { InstancesInfo, WindowInfo, WindowInit } from 'ipc-types';
import { create } from 'zustand';
import {
  appQuitConfirmed,
  instanceSpawn,
  instancesInfo,
  windowFocus,
  windowInit,
  windowList,
  windowNew,
  windowSetConnection,
} from '@/lib/commands';
import { askConfirm } from '@/lib/confirm';
import { subscribeWindow } from '@/lib/events';
import { defineStrings } from '@/lib/i18n';
import { inTauri, invoke } from '@/lib/ipc';
import { errorText } from '@/lib/ipc-error';
import { safeLocalStorage } from '@/lib/storage';
import { toast } from '@/lib/toast';
import { useConnectionsStore } from './connections';
import { isDocDirty, useExtractStore } from './extract';
import { selectActiveJobs, useJobsStore } from './jobs';
import { logLocal } from './logs';
import { useTabsStore } from './tabs';
import { migrateUi, useUiStore } from './ui';
import { useWorkspaceStore } from './workspace';

export const wins = defineStrings('windows', {
  en: {
    windowChip: 'Window {index}',
    windowChipTip: 'Open in window {index}: {title}. Click to bring it to the front.',
    newInstanceTip: 'Starts a separate BulkDB program (process). On macOS `open -n -a BulkDB` does the same.',
    instanceStarted: 'Started another BulkDB',
    otherInstancesTip:
      'Jobs, the concurrent job limit, SSH tunnels and typed passwords are separate per program (process). Connections, settings and history are shared.',
    windowFailed: 'Cannot open a new window: {error}',
    spawnFailed: 'Cannot start another BulkDB: {error}',
    quitTitle: 'Quit BulkDB',
    quitBody: 'Cancel {n} running jobs and quit?',
    quitConfirm: 'Cancel Jobs and Quit',
    closeDirtyTitle: 'Close this window?',
    closeDirtyBody: '{n} queries in this window have unsaved changes.',
    closeDirtyConfirm: 'Close Without Saving',
  },
  ko: {
    windowChip: '창 {index}',
    windowChipTip: '창 {index}에서 열림: {title}. 누르면 그 창을 앞으로 가져옵니다.',
    newInstanceTip: '별도의 BulkDB 프로그램(프로세스)을 실행합니다. macOS에서는 `open -n -a BulkDB`로도 실행할 수 있습니다.',
    instanceStarted: '새 BulkDB를 실행했습니다',
    otherInstancesTip:
      '작업, 동시 작업 수 제한, SSH 터널, 입력한 비밀번호는 프로그램(프로세스)마다 따로입니다. 연결, 설정, 기록은 함께 씁니다.',
    windowFailed: '새 창을 열 수 없습니다: {error}',
    spawnFailed: '새 BulkDB를 실행할 수 없습니다: {error}',
    quitTitle: 'BulkDB 종료',
    quitBody: '실행 중인 작업 {n}개를 취소하고 종료할까요?',
    quitConfirm: '작업 취소 후 종료',
    closeDirtyTitle: '이 창을 닫을까요?',
    closeDirtyBody: '이 창에 저장하지 않은 쿼리가 {n}개 있습니다.',
    closeDirtyConfirm: '저장하지 않고 닫기',
  },
});

/** How often the other-instances chip is refreshed (also on window focus). */
export const INSTANCES_POLL_MS = 15_000;

/**
 * Answer of `window_close_begin`: the shell's `windows::CloseCheck` (a shell-only struct, not
 * a core IPC type, so it has no generated binding; like the `jobs:claimed` payload in
 * `lib/events.ts`).
 */
export interface WindowCloseCheck {
  /** Every window of this process is closing: closing this one ends the session. */
  last: boolean;
  /** Active jobs of this process (the shell cancels them when the session ends). */
  jobs: number;
}

// The window lifecycle commands only this store uses (documented in src-tauri `commands.rs`).
/** This page listens to its window-targeted events now. */
const windowReady = () => invoke<void>('window_ready');
/** This window is about to close: is it the last one, and how many jobs run? */
const windowCloseBegin = () => invoke<WindowCloseCheck>('window_close_begin');
/** This window stays open after all (a close question was declined). */
const windowCloseCancel = () => invoke<void>('window_close_cancel');

export interface WindowsState {
  /** This window (`window_init`); `null` until `initWindows` ran. */
  self: WindowInit | null;
  /** Every window of this process (`windows:changed`), by index. */
  windows: WindowInfo[];
  /** Other BulkDB processes on this config directory. */
  instances: InstancesInfo | null;
  apply(ws: WindowInfo[]): void;
  /** Open a window on Welcome, or on `connId`'s workspace. Errors become a toast. */
  openNew(connId?: string): Promise<void>;
  focus(label: string): Promise<void>;
  /** The other windows that have `connId` open. */
  othersWith(connId: string): WindowInfo[];
  refreshInstances(): Promise<void>;
  spawnInstance(): Promise<void>;
}

export const useWindowsStore = create<WindowsState>()((set, get) => ({
  self: null,
  windows: [],
  instances: null,
  apply: (ws) => set({ windows: [...ws].sort((a, b) => a.index - b.index) }),
  openNew: async (connId) => {
    try {
      await windowNew(connId);
    } catch (e) {
      logLocal('error', `window_new: ${errorText(e)}`);
      toast(wins('windowFailed', { error: errorText(e) }), { tone: 'danger' });
    }
  },
  focus: async (label) => {
    try {
      await windowFocus(label);
    } catch (e) {
      logLocal('error', `window_focus: ${errorText(e)}`);
    }
  },
  othersWith: (connId) => otherWindowsWith(get(), connId),
  refreshInstances: async () => {
    try {
      const info = await instancesInfo();
      const prev = get().instances;
      // Keep the object when nothing changed, so subscribers do not re-render every poll.
      if (!prev || prev.selfPid !== info.selfPid || JSON.stringify(prev.others) !== JSON.stringify(info.others)) set({ instances: info });
    } catch (e) {
      logLocal('error', `instances_info: ${errorText(e)}`);
    }
  },
  spawnInstance: async () => {
    try {
      await instanceSpawn();
      toast(wins('instanceStarted'));
      // Spec 1.2: the 새 BulkDB 실행 item explains that this starts a separate process (and that
      // `open -n -a BulkDB` / the CLI do the same). Tauri menu items carry no tooltip, so surface
      // it here, once per session, the first time the user starts another BulkDB.
      if (!instanceTipShown) {
        instanceTipShown = true;
        toast(wins('newInstanceTip'), { durationMs: 7000 });
      }
      later(2000, () => void get().refreshInstances());
    } catch (e) {
      logLocal('error', `instance_spawn: ${errorText(e)}`);
      toast(wins('spawnFailed', { error: errorText(e) }), { tone: 'danger' });
    }
  },
}));

/** The windows other than this one that show `connId` (pure; for selectors). */
export function otherWindowsWith(s: Pick<WindowsState, 'self' | 'windows'>, connId: string): WindowInfo[] {
  const self = s.self?.label;
  return s.windows.filter((w) => w.connId === connId && w.label !== self);
}

/** Connection ids open in any other window of this process. */
export function connIdsInOtherWindows(s: Pick<WindowsState, 'self' | 'windows'>): string[] {
  const self = s.self?.label;
  return [...new Set(s.windows.filter((w) => w.label !== self && w.connId).map((w) => w.connId as string))];
}

/** Number of other live BulkDB processes (0 while unknown). */
export function otherInstanceCount(s: Pick<WindowsState, 'instances'>): number {
  return s.instances?.others.length ?? 0;
}

// ---- this window's lifecycle ------------------------------------------------------------------

let initialized: Promise<void> | null = null;
/** Bumped by `resetWindowsInit`: an `initWindows` still awaiting then undoes its own wiring. */
let generation = 0;
let cleanups: (() => void)[] = [];
let timers: ReturnType<typeof setTimeout>[] = [];
/** The quit question on screen, if any: at most one per window (see `askQuit`). */
let quitQuestion: Promise<boolean> | null = null;
/** Whether the one-time "새 BulkDB 실행" explanation toast has been shown this session. */
let instanceTipShown = false;

function later(ms: number, fn: () => void): void {
  const h = setTimeout(() => {
    timers = timers.filter((t) => t !== h);
    fn();
  }, ms);
  timers.push(h);
}

/** Unsaved query documents in this window (every workspace it keeps in memory). */
export function dirtyDocsInWindow(): number {
  const docs = useExtractStore.getState().docs;
  return Object.values(useTabsStore.getState().sets)
    .flatMap((set) => set.ids)
    .filter((id) => {
      const doc = docs[id];
      return doc !== undefined && isDocDirty(doc);
    }).length;
}

/**
 * "실행 중인 작업 {n}개를 취소하고 종료할까요?" Only one such dialog is ever shown: a second request
 * while it is open (Cmd+Q again, the OS, closing the last window) shares its answer.
 */
function askQuit(jobs: number): Promise<boolean> {
  if (quitQuestion) return quitQuestion;
  const asked = askConfirm({ title: wins('quitTitle'), body: wins('quitBody', { n: jobs }), confirmLabel: wins('quitConfirm'), danger: true });
  const q = asked.finally(() => {
    if (quitQuestion === q) quitQuestion = null;
  });
  quitQuestion = q;
  return q;
}

/**
 * The window is about to close (`onCloseRequested`): unsaved queries ask first; closing the last
 * window while jobs run asks whether to cancel them and quit (the shell cancels them when the
 * last window is gone). `false` keeps the window open.
 *
 * "Last" is the shell's answer (`window_close_begin`, which marks this window as closing under
 * its registry lock), not this page's copy of `windows:changed`: when several windows close at
 * once (the Windows taskbar's "Close all windows", Option-click on macOS) every copy still lists
 * the others, and an empty or stale copy would ask in a window that is not the last. A `false`
 * answer takes the mark back (`window_close_cancel`).
 */
export async function confirmWindowClose(): Promise<boolean> {
  const dirty = dirtyDocsInWindow();
  if (dirty > 0) {
    const ok = await askConfirm({
      title: wins('closeDirtyTitle'),
      body: wins('closeDirtyBody', { n: dirty }),
      confirmLabel: wins('closeDirtyConfirm'),
      danger: true,
    });
    if (!ok) return false;
  }
  const check = await closeCheck();
  // The shell counts this process's jobs; a job this page shows as running (its status pill)
  // is asked about too: asking once too often is harmless, cancelling without asking is not.
  const jobs = Math.max(check.jobs, selectActiveJobs(useJobsStore.getState()).length);
  if (!check.last || jobs === 0) return true;
  let quit = false;
  try {
    quit = await askQuit(jobs);
  } finally {
    if (!quit) windowCloseCancel().catch((e: unknown) => logLocal('error', `window_close_cancel: ${errorText(e)}`));
  }
  return quit;
}

/** `window_close_begin`; when the shell cannot answer, assume the last window (ask, not cancel). */
async function closeCheck(): Promise<WindowCloseCheck> {
  try {
    return await windowCloseBegin();
  } catch (e) {
    logLocal('error', `window_close_begin: ${errorText(e)}`);
    return { last: true, jobs: 0 };
  }
}

/** `app:quit-requested` (Cmd+Q, the menu, the OS while jobs run): ask, then quit for real. */
export async function handleQuitRequested(jobs: number): Promise<void> {
  // The question is already on screen: whoever asked it acts on the answer.
  if (quitQuestion) return;
  try {
    if (await askQuit(jobs)) await appQuitConfirmed();
  } catch (e) {
    logLocal('error', `app_quit_confirmed: ${errorText(e)}`);
  }
}

/** Tell the shell which connection this window shows whenever the workspace changes. */
function syncConnection(): () => void {
  let sent: string | null | undefined;
  const sync = () => {
    const ws = useWorkspaceStore.getState();
    const connId = ws.mode === 'workspace' ? ws.openConnId : null;
    if (connId === sent) return;
    sent = connId;
    windowSetConnection(connId).catch((e: unknown) => logLocal('error', `window_set_connection: ${errorText(e)}`));
  };
  sync();
  return useWorkspaceStore.subscribe(sync);
}

/**
 * Theme and language follow when another window of this process changes them (db-copy.md 1.1:
 * "so theme and language follow"). ONLY those shared preferences are copied: a full
 * `persist.rehydrate()` merges the whole persisted slice, so hiding the sidebar or dragging a
 * panel divider in one window (which writes `bulkdb-ui` on every pointer move) would reset the
 * sidebar/inspector/panel sizes of every other window — which show different workspaces — on
 * each move. The layout fields stay per-window.
 */
function followSharedUi(): () => void {
  if (typeof window === 'undefined') return () => undefined;
  const key = useUiStore.persist.getOptions().name ?? 'bulkdb-ui';
  const onStorage = (e: StorageEvent) => {
    if (e.key !== key) return;
    // A real storage event carries `newValue`; fall back to reading the key (some callers, and
    // our tests, dispatch the event without it).
    const raw = e.newValue ?? safeLocalStorage().getItem(key);
    if (!raw) return;
    let state: unknown;
    try {
      state = (JSON.parse(raw) as { state?: unknown }).state;
    } catch {
      return;
    }
    const shared = migrateUi(state);
    const ui = useUiStore.getState();
    if (shared.theme !== undefined && shared.theme !== ui.theme) ui.setTheme(shared.theme);
    if (shared.lang !== undefined && shared.lang !== ui.lang) ui.setLang(shared.lang);
  };
  window.addEventListener('storage', onStorage);
  return () => window.removeEventListener('storage', onStorage);
}

/**
 * Wait until the connection list is loaded before opening this window's workspace. `openWorkspace`
 * shows "연결을 찾을 수 없습니다" and bails when the id is not in the list yet, and nothing orders
 * AppEvents' bootstrap `load()` ahead of `initWindows`'s `openWorkspace`. Reuse the in-flight
 * bootstrap load when one is running; start one otherwise.
 */
async function ensureConnectionsLoaded(): Promise<void> {
  const store = useConnectionsStore;
  if (store.getState().loaded) return;
  if (!store.getState().loading) {
    await store.getState().load().catch((e: unknown) => logLocal('error', `conn_list: ${errorText(e)}`));
    return;
  }
  await new Promise<void>((resolve) => {
    const unsub = store.subscribe((s) => {
      if (!s.loading) {
        unsub();
        resolve();
      }
    });
  });
}

/** Refresh the other-instances chip now, every 15 s and when the window gets focus. */
function pollInstances(): () => void {
  const refresh = () => void useWindowsStore.getState().refreshInstances();
  refresh();
  const h = setInterval(refresh, INSTANCES_POLL_MS);
  if (typeof window !== 'undefined') window.addEventListener('focus', refresh);
  return () => {
    clearInterval(h);
    if (typeof window !== 'undefined') window.removeEventListener('focus', refresh);
  };
}

async function guardClose(): Promise<() => void> {
  if (!inTauri()) return () => undefined;
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  return getCurrentWindow().onCloseRequested(async (event) => {
    if (!(await confirmWindowClose())) event.preventDefault();
  });
}

/**
 * Called once by AppShell: learn who this window is (`window_init`), mirror the window list,
 * wire this window's side of the protocol and open the connection it was created for.
 * Idempotent.
 */
export function initWindows(): Promise<void> {
  initialized ??= (async () => {
    const gen = generation;
    const current = () => gen === generation;
    // Keep a cleanup, or run it at once when a reset happened meanwhile.
    const keep = (c: () => void) => {
      if (current()) cleanups.push(c);
      else c();
    };
    try {
      const [self, list] = await Promise.all([windowInit(), windowList().catch(() => [] as WindowInfo[])]);
      if (!current()) return;
      useWindowsStore.setState({ self });
      if (list.length > 0) useWindowsStore.getState().apply(list);
      keep(syncConnection());
      keep(followSharedUi());
      keep(pollInstances());
      keep(await subscribeWindow({ onAppQuitRequested: ({ jobs }) => void handleQuitRequested(jobs) }));
      keep(await guardClose());
      // Every window-targeted listener of this page is attached now: this function's
      // `app:quit-requested` just above, and AppEvents' (tunnel prompts, `jobs:claimed`,
      // `menu:action`), whose `listen` requests went out first: AppEvents mounts before
      // AppShell's effect runs (child effects first) and both wait for the same
      // `currentWindowLabel()` import, which this function only reaches after two round trips.
      // From here on the shell routes to this window and re-sends what it missed meanwhile.
      if (current()) await windowReady().catch((e: unknown) => logLocal('error', `window_ready: ${errorText(e)}`));
      if (current() && self.openConnId) {
        // The workspace cannot open before its connection is in the list, so wait for the load.
        await ensureConnectionsLoaded();
        if (current()) await useWorkspaceStore.getState().openWorkspace(self.openConnId);
      }
    } catch (e) {
      logLocal('error', `window_init: ${errorText(e)}`);
    }
  })();
  return initialized;
}

/** Tests: undo `initWindows` (subscriptions, listeners, timers) so it can run again. */
export function resetWindowsInit(): void {
  generation += 1;
  for (const c of cleanups) c();
  cleanups = [];
  for (const t of timers) clearTimeout(t);
  timers = [];
  quitQuestion = null;
  instanceTipShown = false;
  initialized = null;
}
