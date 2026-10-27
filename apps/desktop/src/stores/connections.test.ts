import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';

describe('connections store', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('save with clearPassword updates the secret flag and forgets the session unlock', async () => {
    const prod = useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')!;
    useConnectionsStore.setState({ unlocked: { 'c-prod': true, 'c-local': true } });
    const saved = await useConnectionsStore.getState().save({ ...prod, clearPassword: true });
    expect(saved.hasPassword).toBe(false);
    const s = useConnectionsStore.getState();
    expect(s.connections.find((c) => c.id === 'c-prod')?.hasPassword).toBe(false);
    expect(s.unlocked).toEqual({ 'c-local': true });
    expect(s.status['c-prod']).toBe('unknown');
    expect(mockBackend.storedSecrets('c-prod').password).toBe(false);
  });

  it('save without clear flags keeps the stored password and the unlock', async () => {
    const prod = useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')!;
    useConnectionsStore.setState({ unlocked: { 'c-prod': true } });
    const saved = await useConnectionsStore.getState().save({ ...prod, name: 'prod renamed' });
    expect(saved.hasPassword).toBe(true);
    expect(useConnectionsStore.getState().unlocked).toEqual({ 'c-prod': true });
  });

  it('refreshConnection re-reads one connection through conn_get', async () => {
    // The list is stale: it claims no password is stored, the backend has one.
    useConnectionsStore.setState((s) => ({ connections: s.connections.map((c) => (c.id === 'c-prod' ? { ...c, hasPassword: false } : c)) }));
    const fresh = await useConnectionsStore.getState().refreshConnection('c-prod');
    expect(fresh.hasPassword).toBe(true);
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')?.hasPassword).toBe(true);
    expect(useConnectionsStore.getState().connections).toHaveLength(2);
    expect(mockBackend.calls).toContain('conn_get');
    await expect(useConnectionsStore.getState().refreshConnection('missing')).rejects.toMatchObject({ code: 'NOT_FOUND' });
  });
});
