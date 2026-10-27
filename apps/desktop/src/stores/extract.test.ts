import { beforeEach, describe, expect, it } from 'vitest';
import { connSave } from '@/lib/commands';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';
import { useExtractStore } from './extract';

/** `#` is a comment for MySQL only, so the `;` behind it splits the text everywhere else. */
const HASH_SQL = 'SELECT * FROM public.orders # ; DELETE FROM orders';

async function addMysql(): Promise<string> {
  const saved = await connSave({ id: '', name: 'shop (MySQL)', driver: 'mysql', host: 'mysql.internal', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false, password: 'pw' });
  useConnectionsStore.setState((s) => ({ connections: [...s.connections, saved] }));
  return saved.id;
}

describe('extract store: SQL check', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('checks with the SQL dialect of the target connection', async () => {
    const mysqlId = await addMysql();
    const s = useExtractStore.getState();
    s.setSql(HASH_SQL);

    // Follows the tree selection (PostgreSQL): two statements.
    const pg = await useExtractStore.getState().check();
    expect(pg?.ok).toBe(false);
    expect(pg?.issues.map((i) => i.code)).toContain('MULTI_STATEMENT');
    expect(useExtractStore.getState().lastCheck).toEqual(pg);

    // The MySQL target reads `#` as a comment: one statement.
    useExtractStore.getState().setConnId(mysqlId);
    const my = await useExtractStore.getState().check();
    expect(my?.ok).toBe(true);
    expect(my?.statements).toBe(1);
    expect(useExtractStore.getState().lastCheck?.ok).toBe(true);
  });

  it('drops the plan and the check when the SQL text changes, keeps them for the same text', async () => {
    useExtractStore.getState().setSql('SELECT * FROM public.orders');
    await useExtractStore.getState().explain();
    await useExtractStore.getState().check();
    expect(useExtractStore.getState().lastPlan).not.toBeNull();
    expect(useExtractStore.getState().lastCheck).not.toBeNull();

    useExtractStore.getState().setSql('SELECT * FROM public.orders');
    expect(useExtractStore.getState().lastPlan).not.toBeNull();

    useExtractStore.getState().setSql('SELECT id FROM public.orders');
    expect(useExtractStore.getState().lastPlan).toBeNull();
    expect(useExtractStore.getState().lastCheck).toBeNull();
  });

  it('forgets the last check when the target connection changes', async () => {
    const mysqlId = await addMysql();
    useExtractStore.getState().setSql('SELECT 1');
    await useExtractStore.getState().check();
    expect(useExtractStore.getState().lastCheck).not.toBeNull();
    useExtractStore.getState().setConnId(mysqlId);
    expect(useExtractStore.getState().lastCheck).toBeNull();
  });

  it('checkSql (live check) uses the target dialect and leaves busy / lastCheck alone', async () => {
    const mysqlId = await addMysql();
    expect((await useExtractStore.getState().checkSql(HASH_SQL))?.ok).toBe(false);
    useExtractStore.getState().setConnId(mysqlId);
    expect((await useExtractStore.getState().checkSql(HASH_SQL))?.ok).toBe(true);
    expect(useExtractStore.getState().busy).toBeNull();
    expect(useExtractStore.getState().lastCheck).toBeNull();
  });

  it('reports a check that could not run (unknown connection) as an error', async () => {
    useExtractStore.setState({ connId: 'gone', sql: 'SELECT 1' });
    expect(await useExtractStore.getState().check()).toBeNull();
    expect(useExtractStore.getState().error).toMatch(/NOT_FOUND/);
    expect(await useExtractStore.getState().checkSql('SELECT 1')).toBeNull();
  });
});
