import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useJobsStore } from '@/stores/jobs';
import { useTabsStore } from '@/stores/tabs';
import { resetStores } from '@/test/stores';
import { setConfirmHandler } from './confirm';
import { canTargetTable, countExactly, execDatabase, openExtractionQuery } from './table-actions';

const orders = { database: 'orders', schema: 'public', name: 'orders' };
const elsewhere = { database: 'analytics_dw', schema: 'public', name: 'events' };

describe('table actions', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, exactCountMs: 20 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('execDatabase: conn.database, else the probed database, else the current catalog database', async () => {
    expect(execDatabase('c-prod')).toBe('orders');
    useConnectionsStore.setState((s) => ({ connections: s.connections.map((c) => (c.id === 'c-prod' ? { ...c, database: undefined } : c)) }));
    expect(execDatabase('c-prod')).toBeNull();
    await useCatalogStore.getState().loadDatabases('c-prod');
    expect(execDatabase('c-prod')).toBe('orders');
  });

  it('canTargetTable refuses a postgres table outside the execution database, allows mysql', async () => {
    expect(canTargetTable('c-prod', orders)).toEqual({ ok: true });
    const refused = canTargetTable('c-prod', elsewhere);
    expect(refused.ok).toBe(false);
    if (!refused.ok) expect(refused.reason).toContain('orders');
    const my = await connSave({ id: '', name: 'shop', driver: 'mysql', host: 'h', database: 'shop', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false, password: 'pw' });
    useConnectionsStore.setState((s) => ({ connections: [...s.connections, my] }));
    expect(canTargetTable(my.id, { database: 'other', name: 't' })).toEqual({ ok: true });
  });

  it('openExtractionQuery opens a pinned table tab in Query view, or refuses with the reason', async () => {
    const id = await openExtractionQuery('c-prod', orders);
    expect(id).not.toBeNull();
    expect(useTabsStore.getState().tabs[id!]).toMatchObject({ kind: 'table', view: 'query', preview: false });
    expect(await openExtractionQuery('c-prod', elsewhere)).toBeNull();
  });

  it('countExactly asks first for ≥ 10M rows (declining sends nothing), not for small tables', async () => {
    await useCatalogStore.getState().loadTables('c-prod', 'orders', 'public');
    const tables = useCatalogStore.getState().tables['c-prod/orders/public'];
    const big = tables.find((t) => Number(t.estimatedRows ?? 0) >= 10_000_000)!;
    const small = tables.find((t) => t.estimatedRows !== undefined && Number(t.estimatedRows) < 10_000_000)!;
    const ask = vi.fn(async () => false);
    setConfirmHandler(ask);
    expect(await countExactly('c-prod', { database: 'orders', schema: big.schema, name: big.name })).toBeNull();
    expect(ask).toHaveBeenCalledTimes(1);
    expect(mockBackend.calls).not.toContain('catalog_exact_count');

    const jobId = await countExactly('c-prod', { database: 'orders', schema: small.schema, name: small.name });
    expect(ask).toHaveBeenCalledTimes(1);
    expect(jobId).toMatch(/^cnt-/);
    expect(Object.values(useCatalogStore.getState().counting)).toContain(jobId);

    ask.mockImplementation(async () => true);
    expect(await countExactly('c-prod', { database: 'orders', schema: big.schema, name: big.name })).toMatch(/^cnt-/);
    // Unknown estimate (table not in the catalog) also asks.
    await countExactly('c-prod', { database: 'orders', schema: 'public', name: 'unknown_table' });
    expect(ask).toHaveBeenCalledTimes(3);
  });

  it('countExactly does not leave a counting flag when the job already finished (race guard)', async () => {
    await useCatalogStore.getState().loadTables('c-prod', 'orders', 'public');
    const small = useCatalogStore.getState().tables['c-prod/orders/public'].find((t) => Number(t.estimatedRows ?? 0) < 10_000_000)!;
    const ref = { database: 'orders', schema: small.schema, name: small.name };
    // The job's terminal state arrived (through events) before the invoke response.
    const original = useJobsStore.getState().jobs;
    useJobsStore.setState({
      jobs: new Proxy(original, {
        get: (target, id: string) =>
          target[id] ?? (id.startsWith('cnt-') ? { summary: { id, state: 'done' }, logs: [], updatedAt: 0 } : undefined),
      }),
    });
    const jobId = await countExactly('c-prod', ref);
    expect(jobId).toMatch(/^cnt-/);
    expect(useCatalogStore.getState().counting).toEqual({});
  });
});
