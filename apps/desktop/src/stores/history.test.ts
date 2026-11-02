import { beforeEach, describe, expect, it } from 'vitest';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { HISTORY_ALL, selectHistoryPage, useHistoryStore } from './history';

describe('history store', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
  });

  it('loads one page per filter through history_list with the connection id (server-side filter)', async () => {
    await useHistoryStore.getState().load('c-prod');
    await useHistoryStore.getState().load(null);
    await useHistoryStore.getState().load('c-local');
    const s = useHistoryStore.getState();
    const prod = selectHistoryPage(s, 'c-prod');
    const all = selectHistoryPage(s, null);
    expect(prod.entries.length).toBeGreaterThan(0);
    expect(prod.entries.every((e) => e.connId === 'c-prod')).toBe(true);
    expect(selectHistoryPage(s, 'c-local').entries.every((e) => e.connId === 'c-local')).toBe(true);
    expect(all.entries.length).toBeGreaterThanOrEqual(prod.entries.length);
    expect(Object.keys(s.lists).sort()).toEqual([HISTORY_ALL, 'c-local', 'c-prod'].sort());
    expect(prod).toMatchObject({ loading: false, error: null });
    // The legacy flat page is untouched by keyed loads.
    expect(s.entries).toEqual([]);
  });

  it('reloads every loaded page', async () => {
    await useHistoryStore.getState().load('c-prod');
    await useHistoryStore.getState().load(null);
    mockBackend.calls.length = 0;
    await useHistoryStore.getState().reloadLoaded();
    expect(mockBackend.calls.filter((c) => c === 'history_list')).toHaveLength(2);
  });

  it('keeps the legacy flat page working (load() without arguments, setConnId)', async () => {
    await useHistoryStore.getState().setConnId('c-prod');
    const s = useHistoryStore.getState();
    expect(s.entries.length).toBeGreaterThan(0);
    expect(s.entries.every((e) => e.connId === 'c-prod')).toBe(true);
    expect(selectHistoryPage(s, 'c-prod').entries).toEqual(s.entries);
  });
});
