import { waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { WindowInfo } from 'ipc-types';
import { registerBuiltinActions, runAction } from '@/lib/actions';
import { setConfirmHandler, type ConfirmOptions } from '@/lib/confirm';
import { listen } from '@/lib/ipc';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { MOCK_MAX_WINDOWS } from '@/lib/mock-windows';
import { useToastStore } from '@/lib/toast';
import { useConnectionsStore } from '@/stores/connections';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import {
  confirmWindowClose,
  connIdsInOtherWindows,
  handleQuitRequested,
  initWindows,
  otherInstanceCount,
  otherWindowsWith,
  resetWindowsInit,
  useWindowsStore,
  wins,
} from './windows';

const calls = (cmd: string) => mockBackend.calls.filter((c) => c === cmd).length;

function seedJob(id: string, state: 'running' | 'done' = 'running') {
  useJobsStore.setState((s) => ({
    jobs: { ...s.jobs, [id]: { summary: { id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: id, startedAt: '2026-10-27T10:00:00Z' }, logs: [], updatedAt: 0 } },
    order: [...s.order, id],
  }));
}

describe('windows store', () => {
  beforeEach(async () => {
    window.history.replaceState(null, '', '/');
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    // AppEvents' part (not mounted here): windows:changed feeds the store.
    await listen<WindowInfo[]>('windows:changed', (ws) => useWindowsStore.getState().apply(ws));
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });
  afterEach(() => {
    resetWindowsInit();
    window.history.replaceState(null, '', '/');
  });

  it('initWindows learns this window, mirrors the list and reports the shown connection', async () => {
    await initWindows();
    const s = useWindowsStore.getState();
    expect(s.self).toEqual({ label: 'main', index: 1 });
    expect(s.windows.map((w) => w.label)).toEqual(['main']);
    // The page listens now: the shell may route tunnel prompts and the quit question here.
    expect(calls('window_ready')).toBe(1);
    expect(mockBackend.calls.indexOf('window_ready')).toBeGreaterThan(mockBackend.calls.indexOf('window_init'));
    // Welcome at start: the shell is told so once (a reloaded page may have shown a workspace).
    expect(calls('window_set_connection')).toBe(1);
    // Opening a workspace retitles the window ("BulkDB — <name>") through windows:changed.
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    await waitFor(() => expect(useWindowsStore.getState().windows[0]).toMatchObject({ connId: 'c-prod', title: 'BulkDB — prod (PostgreSQL)' }));
    useWorkspaceStore.getState().showWelcome();
    await waitFor(() => expect(useWindowsStore.getState().windows[0]?.title).toBe('BulkDB'));
    expect(useWindowsStore.getState().windows[0]?.connId).toBeUndefined();
    // Idempotent.
    await initWindows();
    expect(calls('window_init')).toBe(1);
  });

  it('a window created for a connection opens its workspace', async () => {
    window.history.replaceState(null, '', '/?window=w-2&index=2&conn=c-prod');
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    await initWindows();
    expect(useWindowsStore.getState().self).toEqual({ label: 'w-2', index: 2, openConnId: 'c-prod' });
    expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: 'c-prod' });
  });

  it('openNew asks the shell for a window; other windows with a connection are found', async () => {
    await initWindows();
    await useWindowsStore.getState().openNew('c-prod');
    await waitFor(() => expect(useWindowsStore.getState().windows).toHaveLength(2));
    const s = useWindowsStore.getState();
    expect(otherWindowsWith(s, 'c-prod').map((w) => [w.label, w.index])).toEqual([['w-2', 2]]);
    expect(s.othersWith('c-prod')).toHaveLength(1);
    expect(s.othersWith('c-local')).toEqual([]);
    expect(connIdsInOtherWindows(s)).toEqual(['c-prod']);
    // This window's own connection is not "elsewhere".
    await useWorkspaceStore.getState().openWorkspace('c-local');
    await waitFor(() => expect(useWindowsStore.getState().windows.find((w) => w.label === 'main')?.connId).toBe('c-local'));
    expect(connIdsInOtherWindows(useWindowsStore.getState())).toEqual(['c-prod']);
    await useWindowsStore.getState().focus('w-2');
    expect(calls('window_focus')).toBe(1);
  });

  it('the ninth window is refused with a toast', async () => {
    await initWindows();
    for (let i = 1; i < MOCK_MAX_WINDOWS; i++) await useWindowsStore.getState().openNew();
    expect(useToastStore.getState().toasts).toHaveLength(0);
    await useWindowsStore.getState().openNew();
    const toasts = useToastStore.getState().toasts;
    expect(toasts).toHaveLength(1);
    expect(toasts[0]?.message).toContain('창은 최대 8개까지 열 수 있습니다');
  });

  it('window.new runs openNew (Mod+Shift+N, File → 새 창)', async () => {
    registerBuiltinActions();
    const openNew = vi.fn(async () => undefined);
    useWindowsStore.setState({ openNew });
    await runAction('window.new');
    expect(openNew).toHaveBeenCalledWith();
  });

  it('counts other instances and spawns a new one', async () => {
    await initWindows();
    expect(otherInstanceCount(useWindowsStore.getState())).toBe(0);
    await useWindowsStore.getState().spawnInstance();
    expect(useToastStore.getState().toasts[0]?.message).toBe(wins('instanceStarted'));
    await useWindowsStore.getState().refreshInstances();
    expect(otherInstanceCount(useWindowsStore.getState())).toBe(1);
    // An unchanged answer keeps the same object (no re-render per poll).
    const before = useWindowsStore.getState().instances;
    await useWindowsStore.getState().refreshInstances();
    expect(useWindowsStore.getState().instances).toBe(before);
  });

  it('the theme follows another window of this process (storage event)', async () => {
    await initWindows();
    expect(useUiStore.getState().theme).not.toBe('dark');
    const key = 'bulkdb-ui';
    window.localStorage.setItem(key, JSON.stringify({ state: { ...JSON.parse(window.localStorage.getItem(key) ?? '{"state":{}}').state, theme: 'dark' }, version: 1 }));
    window.dispatchEvent(new StorageEvent('storage', { key }));
    await waitFor(() => expect(useUiStore.getState().theme).toBe('dark'));
    // Other keys are ignored.
    window.localStorage.setItem(key, JSON.stringify({ state: { theme: 'light' }, version: 1 }));
    window.dispatchEvent(new StorageEvent('storage', { key: 'something-else' }));
    await Promise.resolve();
    expect(useUiStore.getState().theme).toBe('dark');
    window.localStorage.removeItem(key);
  });

  it('only theme and language follow another window, never its panel layout', async () => {
    await initWindows();
    // This window's own layout.
    useUiStore.setState({ theme: 'light', sidebarOpen: true, inspectorOpen: false, leftWidth: 240 });
    const key = 'bulkdb-ui';
    // Another window persisted a hidden sidebar, an open inspector, a wider pane, dark theme, en.
    window.localStorage.setItem(
      key,
      JSON.stringify({ state: { theme: 'dark', lang: 'en', sidebarOpen: false, inspectorOpen: true, leftWidth: 400 }, version: 1 }),
    );
    window.dispatchEvent(new StorageEvent('storage', { key, newValue: window.localStorage.getItem(key) }));
    await waitFor(() => expect(useUiStore.getState().theme).toBe('dark'));
    expect(useUiStore.getState().lang).toBe('en');
    // The layout stays this window's own (a full rehydrate would have overwritten all three).
    expect(useUiStore.getState().sidebarOpen).toBe(true);
    expect(useUiStore.getState().inspectorOpen).toBe(false);
    expect(useUiStore.getState().leftWidth).toBe(240);
    window.localStorage.removeItem(key);
    useUiStore.getState().setLang('ko');
  });

  it('waits for the connection list before opening the window connection', async () => {
    window.history.replaceState(null, '', '/?window=w-2&index=2&conn=c-prod');
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    // AppEvents' bootstrap load has not run yet: the list is empty and not loaded.
    useConnectionsStore.setState({ connections: [], drivers: [], loaded: false, loading: false });
    const before = useToastStore.getState().toasts.length;
    await initWindows();
    // It loaded the list first, so the workspace opened instead of toasting "연결을 찾을 수 없습니다".
    expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: 'c-prod' });
    expect(useToastStore.getState().toasts.slice(before).map((t) => t.tone)).not.toContain('danger');
  });

  it('explains 새 BulkDB 실행 once when starting another BulkDB', async () => {
    await initWindows();
    useToastStore.setState({ toasts: [] });
    await useWindowsStore.getState().spawnInstance();
    const msgs = useToastStore.getState().toasts.map((t) => t.message);
    expect(msgs).toContain(wins('instanceStarted'));
    expect(msgs).toContain(wins('newInstanceTip'));
    // Only once per session.
    useToastStore.setState({ toasts: [] });
    await useWindowsStore.getState().spawnInstance();
    expect(useToastStore.getState().toasts.map((t) => t.message)).not.toContain(wins('newInstanceTip'));
  });

  it('app:quit-requested asks and quits only on yes', async () => {
    await initWindows();
    const asked: ConfirmOptions[] = [];
    let answer = false;
    setConfirmHandler((o) => {
      asked.push(o);
      return Promise.resolve(answer);
    });
    mockBackend.emitTo('main', 'app:quit-requested', { jobs: 2 });
    await waitFor(() => expect(asked).toHaveLength(1));
    expect(asked[0]?.body).toBe(wins('quitBody', { n: 2 }));
    await waitFor(() => expect(calls('app_quit_confirmed')).toBe(0));
    // Once that question is answered, a new request asks again.
    await new Promise((r) => setTimeout(r, 0));
    answer = true;
    await handleQuitRequested(3);
    expect(asked).toHaveLength(2);
    expect(calls('app_quit_confirmed')).toBe(1);
  });

  it('only one quit question is ever on screen: repeated requests and the last window closing share it', async () => {
    await initWindows();
    const asked: ConfirmOptions[] = [];
    let answer: (v: boolean) => void = () => undefined;
    setConfirmHandler((o) => {
      asked.push(o);
      return new Promise<boolean>((r) => {
        answer = r;
      });
    });
    mockBackend.emitTo('main', 'app:quit-requested', { jobs: 2 });
    mockBackend.emitTo('main', 'app:quit-requested', { jobs: 2 });
    void handleQuitRequested(2);
    seedJob('j1');
    const closing = confirmWindowClose();
    await new Promise((r) => setTimeout(r, 0));
    expect(asked).toHaveLength(1);
    answer(false);
    expect(await closing).toBe(false);
    expect(calls('app_quit_confirmed')).toBe(0);
    // Answered: the next request asks again, and yes quits once.
    mockBackend.emitTo('main', 'app:quit-requested', { jobs: 1 });
    await waitFor(() => expect(asked).toHaveLength(2));
    answer(true);
    await waitFor(() => expect(calls('app_quit_confirmed')).toBe(1));
  });

  it('closing the last window with running jobs asks; another window open does not', async () => {
    await initWindows();
    const asked: ConfirmOptions[] = [];
    setConfirmHandler((o) => {
      asked.push(o);
      return Promise.resolve(false);
    });
    expect(await confirmWindowClose()).toBe(true);
    seedJob('j1');
    seedJob('j2', 'done');
    expect(await confirmWindowClose()).toBe(false);
    expect(asked.map((o) => o.body)).toEqual([wins('quitBody', { n: 1 })]);
    // With a second window the jobs keep running there: no question.
    await useWindowsStore.getState().openNew();
    await waitFor(() => expect(useWindowsStore.getState().windows).toHaveLength(2));
    expect(await confirmWindowClose()).toBe(true);
    expect(asked).toHaveLength(1);
  });

  it("the shell decides whether this is the last window, not this page's copy of the list", async () => {
    await initWindows();
    seedJob('j1');
    const asked: ConfirmOptions[] = [];
    setConfirmHandler((o) => {
      asked.push(o);
      return Promise.resolve(false);
    });
    // "Close all windows": this copy still lists a window that is closing too (or already gone).
    // The shell knows this one is the last: it asks, and a "no" takes the close mark back.
    useWindowsStore.getState().apply([...useWindowsStore.getState().windows, { label: 'w-9', title: 'BulkDB', focused: false, index: 2 }]);
    expect(await confirmWindowClose()).toBe(false);
    expect(asked.map((o) => o.body)).toEqual([wins('quitBody', { n: 1 })]);
    expect(calls('window_close_begin')).toBe(1);
    await waitFor(() => expect(calls('window_close_cancel')).toBe(1));
    // The reverse: an empty copy (window_list failed) while another window is open. Not the
    // last window: no question, the jobs keep running in the other one.
    await useWindowsStore.getState().openNew();
    await waitFor(() => expect(useWindowsStore.getState().windows).toHaveLength(2));
    useWindowsStore.setState({ windows: [] });
    expect(await confirmWindowClose()).toBe(true);
    expect(asked).toHaveLength(1);
    expect(calls('window_close_cancel')).toBe(1);
  });

  it('a reset during initWindows undoes its wiring', async () => {
    const pending = initWindows();
    resetWindowsInit();
    await pending;
    const before = calls('window_set_connection');
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    await Promise.resolve();
    expect(calls('window_set_connection')).toBe(before);
  });
});
