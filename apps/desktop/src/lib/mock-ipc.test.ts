import type { ConnectionInput, ExportRequest, HostKeyPrompt, JobDone, JobStateEvent, PlanSummary, PreflightReport, ProbeResult } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { catalogInvalidate, connGet, connSave, exportPreflight, exportStart, jobCancel, jobResume, queryExplain, sqlCheck } from './commands';
import { listen } from './ipc';
import { mockBackend } from './mock-ipc';

const FAST = { latencyMs: 0, progressTickMs: 5, prepareMs: 10, runMs: 60, finishingMs: 5, serverIntervalMs: 20, exactCountMs: 20 };

function req(overrides: Partial<ExportRequest> = {}): ExportRequest {
  return {
    connId: 'c-prod',
    sql: 'SELECT * FROM public.orders',
    session: { workMem: '64MB' },
    output: { dir: 'C:\\exports', namePattern: '{conn}_{name}', format: 'csv', csv: { header: true, delimiter: ',', nullAs: '', bom: false, crlf: true }, overwrite: false },
    ...overrides,
  };
}

async function collectStates(): Promise<{ states: JobStateEvent[]; done: JobDone[] }> {
  const states: JobStateEvent[] = [];
  const done: JobDone[] = [];
  await listen<JobStateEvent>('job:state', (e) => states.push(e));
  await listen<JobDone>('job:done', (e) => done.push(e));
  return { states, done };
}

describe('mock backend', () => {
  beforeEach(() => {
    mockBackend.reset();
    mockBackend.configure(FAST);
  });

  it('sql_check rejects multiple statements and non-SELECT statements', async () => {
    const multi = await sqlCheck('select 1; select 2');
    expect(multi.ok).toBe(false);
    expect(multi.issues.map((i) => i.code)).toContain('MULTI_STATEMENT');
    const dml = await sqlCheck('DELETE FROM orders');
    expect(dml.ok).toBe(false);
    expect(dml.issues.map((i) => i.code)).toContain('SQL_NOT_SELECT');
    const cte = await sqlCheck("WITH x AS (SELECT 1) SELECT * FROM x; -- trailing comment");
    expect(cte.ok).toBe(true);
    expect(cte.kind).toBe('with');
    expect(cte.normalized).toBe('WITH x AS (SELECT 1) SELECT * FROM x');
    expect((await sqlCheck('')).issues[0].code).toBe('SQL_EMPTY');
  });

  it('query_explain warns about LATERAL', async () => {
    const plan: PlanSummary = await queryExplain('c-prod', 'SELECT * FROM public.orders o, LATERAL (SELECT 1) x');
    expect(plan.warnings.map((w) => w.code)).toContain('LATERAL_PATTERN');
    expect(plan.root?.nodeType).toBe('Nested Loop');
    const plain = await queryExplain('c-prod', 'SELECT * FROM public.orders WHERE id = 1');
    expect(plain.warnings.find((w) => w.code === 'LATERAL_PATTERN')).toBeUndefined();
    // Like core: a guard refusal is INVALID and never reaches the server.
    await expect(queryExplain('c-prod', 'DROP TABLE x')).rejects.toMatchObject({ code: 'INVALID' });
  });

  it('sql_check, query_explain and preflight read the query with the connection dialect', async () => {
    const mysql = await connSave({ id: '', name: 'shop (MySQL)', driver: 'mysql', host: 'mysql.internal', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false, password: 'pw' });
    const hash = 'SELECT * FROM public.orders # ; DELETE FROM orders';
    const sneaky = String.raw`SELECT '\'' ; DELETE FROM orders -- '`;

    // No connection / PostgreSQL: `#` is not a comment, `\` is not an escape.
    expect((await sqlCheck(hash)).issues.map((i) => i.code)).toContain('MULTI_STATEMENT');
    expect((await sqlCheck(hash, 'c-prod')).ok).toBe(false);
    expect((await sqlCheck(sneaky, 'c-prod')).ok).toBe(true);
    // MySQL: the other way round.
    const my = await sqlCheck(hash, mysql.id);
    expect(my.ok).toBe(true);
    expect(my.statements).toBe(1);
    const mySneaky = await sqlCheck(sneaky, mysql.id);
    expect(mySneaky.ok).toBe(false);
    expect(mySneaky.issues.map((i) => i.code)).toContain('MULTI_STATEMENT');
    expect((await sqlCheck('/*!50000 SELECT 1 */', mysql.id)).issues[0].code).toBe('SQL_EXEC_COMMENT');
    // Unknown back end (ODBC): every reading must pass.
    const odbc = await connSave({ id: '', name: 'dsn', driver: 'odbc', fields: { connection_string: 'DSN=x' }, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false });
    expect((await sqlCheck(sneaky, odbc.id)).ok).toBe(false);
    await expect(sqlCheck('select 1', 'missing')).rejects.toMatchObject({ code: 'NOT_FOUND' });

    await expect(queryExplain(mysql.id, sneaky)).rejects.toMatchObject({ code: 'INVALID' });
    expect((await queryExplain(mysql.id, hash)).raw).toBeDefined();
    const report = await exportPreflight(req({ connId: mysql.id, sql: sneaky }));
    expect(report.blockers.map((b) => b.code)).toContain('MULTI_STATEMENT');
  });

  it('conn_save clears exactly the stored secret it is asked to; conn_get re-reads the flags', async () => {
    const base: ConnectionInput = {
      id: '',
      name: 'tunnelled',
      driver: 'postgres',
      host: 'db',
      fields: {},
      ssh: { enabled: true, host: 'bastion', port: 22, user: 'deploy', auth: 'password', compression: true },
      session: {},
      savePassword: true,
      hasPassword: false,
      hasSshSecret: false,
    };
    const saved = await connSave({ ...base, password: 'pw', sshPassword: 'sp' });
    expect(saved.hasPassword && saved.hasSshSecret).toBe(true);
    const edit: ConnectionInput = { ...saved };

    const noPassword = await connSave({ ...edit, clearPassword: true });
    expect(noPassword.hasPassword).toBe(false);
    expect(noPassword.hasSshSecret).toBe(true);
    expect(mockBackend.storedSecrets(saved.id)).toEqual({ password: false, sessionPassword: false, sshSecret: true });

    const noSsh = await connSave({ ...edit, clearSshSecret: true });
    expect(noSsh.hasSshSecret).toBe(false);
    expect((await connGet(saved.id)).hasSshSecret).toBe(false);
    await expect(connGet('missing')).rejects.toMatchObject({ code: 'NOT_FOUND' });

    await expect(connSave({ ...edit, password: 'new', clearPassword: true })).rejects.toMatchObject({ code: 'INVALID' });
    await expect(connSave({ ...edit, sshPassword: 'new', clearSshSecret: true })).rejects.toMatchObject({ code: 'INVALID' });
  });

  it('catalog_invalidate resolves for any connection id, like core', async () => {
    await expect(catalogInvalidate('c-prod')).resolves.toBeUndefined();
    await expect(catalogInvalidate('missing')).resolves.toBeUndefined();
    expect(mockBackend.calls.filter((c) => c === 'catalog_invalidate')).toHaveLength(2);
  });

  it('export_preflight warns on high work_mem and on overwrite', async () => {
    const high: PreflightReport = await exportPreflight(req({ session: { workMem: '256MB' } }));
    expect(high.warnings.map((w) => w.code)).toContain('WORK_MEM_HIGH');
    expect(high.blockers).toHaveLength(0);
    const ok = await exportPreflight(req());
    expect(ok.warnings.map((w) => w.code)).not.toContain('WORK_MEM_HIGH');
    expect(ok.warnings).toHaveLength(0);
    expect(ok.estimates.outputPath).toMatch(/\.csv$/);
    const over = await exportPreflight(req({ output: { ...req().output, overwrite: true } }));
    expect(over.warnings.map((w) => w.code)).toContain('OVERWRITE');
    const bad = await exportPreflight(req({ sql: 'UPDATE orders SET x = 1' }));
    expect(bad.blockers.map((b) => b.code)).toContain('SQL_NOT_SELECT');
  });

  it('export_start drives a job pending -> preflight -> running -> finishing -> done', async () => {
    const { states, done } = await collectStates();
    const jobId = await exportStart(req());
    await vi.waitFor(() => expect(done).toHaveLength(1), { timeout: 3000 });
    expect(states.map((s) => s.state)).toEqual(['pending', 'preflight', 'running', 'finishing', 'done']);
    expect(done[0].jobId).toBe(jobId);
    expect(done[0].result?.files[0]).toMatch(/prod_PostgreSQL_orders\.csv$/);
    expect(Number(done[0].result?.rows)).toBeGreaterThan(0);
    expect(mockBackend.getJob(jobId)?.state).toBe('done');
  });

  it('stops in warned until job_resume acknowledges the warnings', async () => {
    const { states } = await collectStates();
    const jobId = await exportStart(req({ session: { workMem: '512MB' } }));
    await vi.waitFor(() => expect(states.map((s) => s.state)).toContain('warned'));
    expect(mockBackend.getJob(jobId)?.preflight?.warnings.map((w) => w.code)).toEqual(['WORK_MEM_HIGH']);
    await expect(jobResume(jobId, [])).rejects.toMatchObject({ code: 'INVALID' });
    await jobResume(jobId, ['WORK_MEM_HIGH']);
    await vi.waitFor(() => expect(mockBackend.getJob(jobId)?.state).toBe('done'), { timeout: 3000 });
  });

  it('rejects a blocked request with job:done rejected', async () => {
    const { states, done } = await collectStates();
    await exportStart(req({ sql: 'select 1; select 2' }));
    await vi.waitFor(() => expect(done).toHaveLength(1));
    expect(states.map((s) => s.state)).toEqual(['pending', 'preflight', 'rejected']);
    expect(done[0].error?.code).toBe('INVALID');
  });

  it('job_cancel yields cancelled with a .partial file', async () => {
    const { states, done } = await collectStates();
    const jobId = await exportStart(req());
    await vi.waitFor(() => expect(states.map((s) => s.state)).toContain('running'));
    await jobCancel(jobId);
    await vi.waitFor(() => expect(done).toHaveLength(1));
    expect(done[0].state).toBe('cancelled');
    expect(done[0].error?.code).toBe('CANCELLED');
    expect(done[0].result?.files[0]).toMatch(/\.partial$/);
    await expect(jobCancel(jobId)).rejects.toMatchObject({ code: 'INVALID' });
  });

  it('asks for the host key once on the ssh connection', async () => {
    const prompts: HostKeyPrompt[] = [];
    await listen<HostKeyPrompt>('tunnel:hostkey', (p) => prompts.push(p));
    const { connTest, tunnelConfirmHostkey } = await import('./commands');
    const first = connTest('c-prod');
    await vi.waitFor(() => expect(prompts).toHaveLength(1));
    expect(prompts[0].fingerprint).toMatch(/^SHA256:/);
    await tunnelConfirmHostkey(prompts[0].promptId, true);
    const probe: ProbeResult = await first;
    expect(probe.tunnelUsed).toBe(true);
    expect(probe.steps).toEqual(['tunnel', 'connect', 'probe']);
    expect(mockBackend.hostKnown('bastion.example.com')).toBe(true);
    const second = await connTest('c-prod');
    expect(prompts).toHaveLength(1);
    expect(second.server.driver).toBe('postgres');
  });

  it('exact count runs as a job and reports through job:done', async () => {
    const { done } = await collectStates();
    const { catalogExactCount } = await import('./commands');
    const jobId = await catalogExactCount('c-prod', { schema: 'public', name: 'roles' });
    await vi.waitFor(() => expect(done).toHaveLength(1));
    expect(done[0].jobId).toBe(jobId);
    expect(done[0].exactCount?.table.name).toBe('roles');
    expect(Number(done[0].exactCount?.rows)).toBe(18);
  });

  it('remembers finished exact counts for catalog_last_exact_count, like core history', async () => {
    const { done } = await collectStates();
    const { catalogExactCount, catalogLastExactCount } = await import('./commands');
    const roles = { schema: 'public', name: 'roles' };
    expect(await catalogLastExactCount('c-prod', roles)).toBeNull();
    const jobId = await catalogExactCount('c-prod', roles);
    await vi.waitFor(() => expect(done).toHaveLength(1));
    const stored = await catalogLastExactCount('c-prod', roles);
    expect(stored).toEqual(done[0].exactCount);
    expect(mockBackend.getJob(jobId)?.exactCount).toEqual(stored);
    // Same key as core `table_key`: an empty database part does not make it another table.
    expect(await catalogLastExactCount('c-prod', { database: '', schema: 'public', name: 'roles' })).toMatchObject({ rows: stored?.rows, at: stored?.at });
    // Per connection and per table; unknown connections are NOT_FOUND.
    expect(await catalogLastExactCount('c-local', roles)).toBeNull();
    expect(await catalogLastExactCount('c-prod', { schema: 'public', name: 'orders' })).toBeNull();
    await expect(catalogLastExactCount('nope', roles)).rejects.toMatchObject({ code: 'NOT_FOUND' });
    // A later count replaces the stored one.
    const at = '2099-01-01T00:00:00.000Z';
    mockBackend.seedExactCount('c-prod', { table: roles, rows: BigInt(42), elapsedMs: BigInt(1), at });
    expect(await catalogLastExactCount('c-prod', roles)).toMatchObject({ rows: BigInt(42), at });
  });
});
