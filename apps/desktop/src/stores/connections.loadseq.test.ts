import type { Connection } from 'ipc-types';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// `load()` has a request counter so a slow older response never overwrites a newer list
// (connections:changed can start several overlapping loads; conn_list does a keyring lookup per
// connection, so responses can arrive out of order). Drive conn_list directly to force that race.
vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, connList: vi.fn(actual.connList) };
});

import { connList } from '@/lib/commands';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';

function conn(id: string): Connection {
  return { id, name: id.toUpperCase(), driver: 'sqlite', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false };
}

describe('connections load() request counter', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('a slow older load() never overwrites a newer list', async () => {
    let releaseA!: () => void;
    const aGate = new Promise<void>((r) => (releaseA = r));
    vi.mocked(connList)
      .mockImplementationOnce(async () => {
        await aGate;
        return [conn('a')];
      })
      .mockImplementationOnce(async () => [conn('b')]);

    const p1 = useConnectionsStore.getState().load(); // seq 1, blocked on the gate
    const p2 = useConnectionsStore.getState().load(); // seq 2, resolves immediately
    await p2;
    expect(useConnectionsStore.getState().connections.map((c) => c.id)).toEqual(['b']);

    releaseA();
    await p1;
    // The late seq-1 response is ignored: the newer list stands, and loading is settled.
    expect(useConnectionsStore.getState().connections.map((c) => c.id)).toEqual(['b']);
    expect(useConnectionsStore.getState().loading).toBe(false);
  });
});
