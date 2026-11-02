import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connGet, connSave } from '@/lib/commands';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { selectTabList, useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { setConfirmHandler } from './confirm';
import {
  afterConnectionSaved,
  clearWriteAllowed,
  deleteConnection,
  duplicateConnection,
  openQueryInConnection,
  rescanDrivers,
  testConnection,
} from './connection-actions';
import { resumeDbPasswordRequests } from './db-password';
import { connTag } from './env';
import { useToastStore } from './toast';

describe('connection actions', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('rescanDrivers returns the available count and toasts it', async () => {
    mockBackend.installDriver('oracle');
    const count = await rescanDrivers();
    expect(count).toBe(useConnectionsStore.getState().drivers.filter((d) => d.available).length);
    expect(useToastStore.getState().toasts.at(-1)?.message).toContain(String(count));
  });

  it('a duplicated keyring connection is "ask every time": it prompts instead of failing with AUTH', async () => {
    const copy = await duplicateConnection('c-prod');
    expect(copy).not.toBeNull();
    expect(copy!.id).not.toBe('c-prod');
    expect(copy!.name).toBe('prod (PostgreSQL) 복사본');
    expect(copy!.savePassword).toBe(false);
    expect(copy!.hasPassword).toBe(false);
    expect(connTag(copy!)).toBe('production');
    expect(needsSessionPassword(useConnectionsStore.getState(), copy!.id)).toBe(true);
    expect(await testConnection(copy!.id)).toBeNull();
    expect(useUiStore.getState().dbPasswordPrompts).toMatchObject([{ connId: copy!.id, requests: [{ reason: 'test' }] }]);
    expect(mockBackend.calls).not.toContain('conn_test');
    expect(useToastStore.getState().toasts.at(-1)?.message).toMatch(/비밀번호는 복사되지 않았습니다/);
  });

  it('deleteConnection confirms once, removes it and closes its workspace', async () => {
    await useWorkspaceStore.getState().openWorkspace('c-local');
    const ask = vi.fn(async () => false);
    setConfirmHandler(ask);
    expect(await deleteConnection('c-local')).toBe(false);
    expect(useConnectionsStore.getState().connections.some((c) => c.id === 'c-local')).toBe(true);
    ask.mockImplementation(async () => true);
    expect(await deleteConnection('c-local')).toBe(true);
    expect(ask).toHaveBeenCalledTimes(2);
    expect(useConnectionsStore.getState().connections.some((c) => c.id === 'c-local')).toBe(false);
    expect(useWorkspaceStore.getState().mode).toBe('welcome');
    expect(useTabsStore.getState().sets['c-local']).toBeUndefined();
  });

  it('afterConnectionSaved reloads metadata only for an open workspace, keeping the SQL', async () => {
    await afterConnectionSaved('c-local');
    expect(mockBackend.calls).not.toContain('catalog_invalidate');
    await useWorkspaceStore.getState().openWorkspace('c-local');
    const tab = selectTabList(useTabsStore.getState(), 'c-local')[0].id;
    useExtractStore.getState().setSql(tab, 'SELECT 1');
    await afterConnectionSaved('c-local');
    expect(mockBackend.calls).toContain('catalog_invalidate');
    expect(useExtractStore.getState().docs[tab].sql).toBe('SELECT 1');
    expect(useWorkspaceStore.getState().scope['c-local']).toEqual({ database: 'main', schema: 'main' });
  });

  it('openQueryInConnection copies SQL and output into a new tab of the target, with the target session', async () => {
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    const src = selectTabList(useTabsStore.getState(), 'c-prod')[0].id;
    useExtractStore.getState().setSql(src, 'SELECT * FROM t');
    useExtractStore.getState().setName(src, 'named');
    useExtractStore.getState().setOutput(src, { namePattern: '{name}_x' });
    const id = await openQueryInConnection(src, 'c-local');
    expect(useWorkspaceStore.getState().openConnId).toBe('c-local');
    const doc = useExtractStore.getState().docs[id!];
    expect(doc).toMatchObject({ connId: 'c-local', sql: 'SELECT * FROM t', name: 'named', session: { fetchBatchRows: 65536 } });
    expect(doc.output.namePattern).toBe('{name}_x');
    expect(selectTabList(useTabsStore.getState(), 'c-local')).toHaveLength(1);
  });

  it('openQueryInConnection behind an "ask every time" password opens only the copied tab', async () => {
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    const src = selectTabList(useTabsStore.getState(), 'c-prod')[0].id;
    useExtractStore.getState().setSql(src, 'SELECT 2');
    useExtractStore.getState().setName(src, 'copied');
    // A plain "ask every time" PostgreSQL connection (no tunnel, so no host-key prompt).
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
    const target = saved.id;
    expect(await openQueryInConnection(src, target)).toBeNull();
    const prompt = useUiStore.getState().dbPasswordPrompts.find((p) => p.connId === target)!;
    expect(prompt.requests).toMatchObject([{ reason: 'open' }]);
    await useConnectionsStore.getState().test(target, 'pw');
    useUiStore.getState().resolveDbPassword(prompt.id);
    await resumeDbPasswordRequests(prompt);
    expect(useWorkspaceStore.getState().openConnId).toBe(target);
    const tabs = selectTabList(useTabsStore.getState(), target);
    expect(tabs.map((t) => useExtractStore.getState().docs[t.id]?.name)).toEqual(['copied']);
  });

  it('clearWriteAllowed re-reads the stored connection before saving (not the stale list copy)', async () => {
    let saved = await connSave({ id: '', name: 'tgt', driver: 'postgres', host: 'db', user: 'u', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false });
    saved = await connSave({ ...saved, writeAllowed: true }); // write-allowed only on an existing id
    useConnectionsStore.setState({ connections: [saved] });
    // A stale Welcome-list copy: a different (older) name than what is stored now.
    await clearWriteAllowed({ ...saved, name: 'tgt-old' });
    const stored = await connGet(saved.id);
    expect(stored.writeAllowed).toBeFalsy();
    // The save used the re-read connection, so the stale name never reached the backend.
    expect(stored.name).toBe('tgt');
  });

  it('a duplicate never inherits writeAllowed', async () => {
    const conn = { ...mockConnections().find((c) => c.id === 'c-staging')!, writeAllowed: true };
    useConnectionsStore.setState({ connections: [conn] });
    const save = vi.spyOn(useConnectionsStore.getState(), 'save');
    await duplicateConnection(conn.id);
    expect(save).toHaveBeenCalledOnce();
    expect('writeAllowed' in (save.mock.calls[0][0] as object)).toBe(false);
  });
});
