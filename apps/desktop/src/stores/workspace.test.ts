import type { HistoryEntry } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { setConfirmHandler } from '@/lib/confirm';
import { resumeDbPasswordRequests } from '@/lib/db-password';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';
import { useExtractStore } from './extract';
import { useSettingsStore } from './settings';
import { selectTabList, useTabsStore } from './tabs';
import { useUiStore } from './ui';
import { useWorkspaceStore } from './workspace';

const ws = () => useWorkspaceStore.getState();

async function askEveryTime(): Promise<string> {
  const saved = await connSave({
    id: '',
    name: 'ask (PG)',
    driver: 'postgres',
    host: 'db.internal',
    user: 'analyst',
    fields: {},
    session: {},
    savePassword: false,
    hasPassword: false,
    hasSshSecret: false,
  });
  useConnectionsStore.setState((s) => ({ connections: [...s.connections, saved] }));
  return saved.id;
}

function entry(connId: string): HistoryEntry {
  return {
    id: BigInt(1),
    jobId: 'job-x',
    connId,
    startedAt: '2026-10-27T01:02:03Z',
    state: 'done',
    label: 'orders_oct',
    sql: 'SELECT * FROM public.orders',
    session: { workMem: '256MB' },
    output: { dir: 'D:\\out', namePattern: '{name}', format: 'csv', overwrite: false },
    files: [],
  };
}

describe('workspace store', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: null, loaded: true });
  });

  it('starts on Welcome; an unknown id toasts and stays there', async () => {
    expect(ws().mode).toBe('welcome');
    expect(await ws().openWorkspace('nope')).toBe(false);
    expect(ws().mode).toBe('welcome');
    expect(useToastStore.getState().toasts.map((t) => t.message)).toEqual(['연결을 찾을 수 없습니다']);
  });

  it('switches synchronously, then creates one query tab after the settings and probes the connection', async () => {
    const p = ws().openWorkspace('c-prod');
    expect(ws().mode).toBe('workspace');
    expect(ws().openConnId).toBe('c-prod');
    expect(useConnectionsStore.getState().selectedId).toBe('c-prod');
    expect(useTabsStore.getState().workspaceConnId).toBe('c-prod');
    expect(await p).toBe(true);
    expect(selectTabList(useTabsStore.getState(), 'c-prod')).toHaveLength(1);
    // The doc took the settings' defaults (loaded before it was created).
    const doc = Object.values(useExtractStore.getState().docs)[0];
    expect(useSettingsStore.getState().settings).not.toBeNull();
    expect(doc.output.dir).toBe('C:\\exports');
    // The probe runs (it waits for the SSH host key prompt here).
    expect(mockBackend.calls).toContain('conn_test');
    await vi.waitFor(() => expect(ws().scope['c-prod']).toEqual({ database: 'orders', schema: 'public' }));
    // Reopening keeps the tabs (no second empty tab).
    await ws().openWorkspace('c-prod');
    expect(selectTabList(useTabsStore.getState(), 'c-prod')).toHaveLength(1);
  });

  it('clicking A then B lands on B (only per-connection state is written after the await)', async () => {
    const a = ws().openWorkspace('c-prod');
    const b = ws().openWorkspace('c-local');
    await Promise.all([a, b]);
    expect(ws().openConnId).toBe('c-local');
    expect(ws().opened).toEqual(['c-local', 'c-prod']);
    expect(useConnectionsStore.getState().selectedId).toBe('c-local');
  });

  it('asks for an "ask every time" password before opening', async () => {
    const id = await askEveryTime();
    expect(await ws().openWorkspace(id)).toBe(false);
    expect(ws().mode).toBe('welcome');
    expect(useUiStore.getState().dbPasswordPrompts).toMatchObject([{ connId: id, requests: [{ reason: 'open' }] }]);
  });

  it('ensureScope picks the schema named like the user (case-insensitive), else the driver default', async () => {
    useConnectionsStore.setState((s) => ({ connections: s.connections.map((c) => (c.id === 'c-prod' ? { ...c, user: 'ANALYTICS' } : c)) }));
    await ws().ensureScope('c-prod');
    expect(ws().scope['c-prod']).toEqual({ database: 'orders', schema: 'analytics' });
    // A new database clears the schema and applies the rule again.
    ws().setScope('c-prod', { database: 'analytics_dw' });
    expect(ws().scope['c-prod']).toEqual({ database: 'analytics_dw', schema: null });
    await vi.waitFor(() => expect(ws().scope['c-prod']).toEqual({ database: 'analytics_dw', schema: 'public' }));
    // A valid existing scope is kept.
    ws().setScope('c-prod', { schema: 'staging' });
    await ws().ensureScope('c-prod');
    expect(ws().scope['c-prod']).toEqual({ database: 'analytics_dw', schema: 'staging' });
    // SQLite: main.
    await ws().ensureScope('c-local');
    expect(ws().scope['c-local']).toEqual({ database: 'main', schema: 'main' });
  });

  it('closeWorkspace asks when a query is dirty, unless forced; closing the open one shows Welcome', async () => {
    await ws().openWorkspace('c-prod');
    const tabId = selectTabList(useTabsStore.getState(), 'c-prod')[0].id;
    useExtractStore.getState().setSql(tabId, 'SELECT 1');
    const ask = vi.fn(async () => false);
    setConfirmHandler(ask);
    expect(await ws().closeWorkspace('c-prod')).toBe(false);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ws().mode).toBe('workspace');

    expect(await ws().closeWorkspace('c-prod', { force: true })).toBe(true);
    expect(ask).toHaveBeenCalledTimes(1);
    expect(ws().mode).toBe('welcome');
    expect(ws().openConnId).toBeNull();
    expect(ws().opened).toEqual([]);
    expect(useTabsStore.getState().sets['c-prod']).toBeUndefined();
    expect(useExtractStore.getState().docs[tabId]).toBeUndefined();
  });

  it('showWelcome keeps workspaces in memory', async () => {
    await ws().openWorkspace('c-prod');
    ws().showWelcome();
    expect(ws().mode).toBe('welcome');
    expect(ws().opened).toEqual(['c-prod']);
    expect(useTabsStore.getState().workspaceConnId).toBeNull();
    expect(selectTabList(useTabsStore.getState(), 'c-prod')).toHaveLength(1);
  });

  it('openFromHistory opens the entry in a new query tab of its workspace (and can run it)', async () => {
    const tabId = await ws().openFromHistory(entry('c-prod'));
    expect(tabId).not.toBeNull();
    expect(ws().openConnId).toBe('c-prod');
    // No extra empty tab next to it.
    expect(selectTabList(useTabsStore.getState(), 'c-prod').map((t) => t.id)).toEqual([tabId]);
    const doc = useExtractStore.getState().docs[tabId!];
    expect(doc).toMatchObject({ sql: 'SELECT * FROM public.orders', name: 'orders_oct', session: { workMem: '256MB' } });
    expect(doc.output.dir).toBe('D:\\out');

    const runTab = await ws().openFromHistory(entry('c-prod'), { run: true });
    await vi.waitFor(() => expect(useExtractStore.getState().docs[runTab!].lastPreflight).not.toBeNull());
  });

  it('openFromHistory: unknown connection → toast, null', async () => {
    expect(await ws().openFromHistory(entry('gone'))).toBeNull();
    expect(useToastStore.getState().toasts).toHaveLength(1);
  });

  it('openFromHistory waits for the password, then finishes through the prompt', async () => {
    const id = await askEveryTime();
    expect(await ws().openFromHistory(entry(id))).toBeNull();
    const prompt = useUiStore.getState().dbPasswordPrompts[0];
    expect(prompt.requests[0]).toMatchObject({ reason: 'open' });
    // The dialog accepted the password (conn_test), then resumes.
    await useConnectionsStore.getState().test(id, 'pw');
    useUiStore.getState().resolveDbPassword(prompt.id);
    await resumeDbPasswordRequests(prompt);
    await vi.waitFor(() =>
      expect(selectTabList(useTabsStore.getState(), id).some((t) => useExtractStore.getState().docs[t.id]?.name === 'orders_oct')).toBe(true),
    );
    expect(ws().openConnId).toBe(id);
    // Only the history tab: no empty "쿼리 1" next to it.
    expect(selectTabList(useTabsStore.getState(), id).map((t) => useExtractStore.getState().docs[t.id]?.name)).toEqual(['orders_oct']);
  });

  it('a plain open behind the password still gets the empty query tab', async () => {
    const id = await askEveryTime();
    expect(await ws().openWorkspace(id)).toBe(false);
    const plain = useUiStore.getState().dbPasswordPrompts[0];
    await useConnectionsStore.getState().test(id, 'pw');
    useUiStore.getState().resolveDbPassword(plain.id);
    await resumeDbPasswordRequests(plain);
    expect(ws().openConnId).toBe(id);
    expect(selectTabList(useTabsStore.getState(), id)).toHaveLength(1);
  });

  it('a plain open and a history open merged into one prompt leave only the history tab', async () => {
    const id2 = await askEveryTime();
    await ws().openWorkspace(id2);
    await ws().openFromHistory(entry(id2));
    const merged = useUiStore.getState().dbPasswordPrompts[0];
    expect(merged.requests.map((r) => r.reason)).toEqual(['open', 'open']);
    await useConnectionsStore.getState().test(id2, 'pw');
    useUiStore.getState().resolveDbPassword(merged.id);
    await resumeDbPasswordRequests(merged);
    await vi.waitFor(() => expect(selectTabList(useTabsStore.getState(), id2)).toHaveLength(1));
    const only = selectTabList(useTabsStore.getState(), id2)[0];
    expect(useExtractStore.getState().docs[only.id]?.name).toBe('orders_oct');
  });
});
