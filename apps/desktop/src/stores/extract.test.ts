import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { mockConnections, mockDrivers, mockSettings } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';
import { defaultOutputOpts, isDocDirty, LEGACY_DOC_ID, useExtractStore } from './extract';
import { useSettingsStore } from './settings';

/** `#` is a comment for MySQL only, so the `;` behind it splits the text everywhere else. */
const HASH_SQL = 'SELECT * FROM public.orders # ; DELETE FROM orders';
const ORDERS = 'SELECT * FROM public.orders';

const ex = () => useExtractStore.getState();

async function addMysql(): Promise<string> {
  const saved = await connSave({ id: '', name: 'shop (MySQL)', driver: 'mysql', host: 'mysql.internal', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false, password: 'pw' });
  useConnectionsStore.setState((s) => ({ connections: [...s.connections, saved] }));
  return saved.id;
}

describe('extract store: per-tab documents', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('createDoc reads connection and settings defaults at call time', () => {
    const before = ex().createDoc('q1', 'c-local');
    expect(before.output.dir).toBe('');
    useSettingsStore.setState({ settings: { ...mockSettings(), defaultOutputDir: 'D:\\def', csvCrlfDefault: false } });
    const local = ex().createDoc('q2', 'c-local');
    expect(local.output.dir).toBe('D:\\def');
    expect(local.output.csv?.crlf).toBe(false);
    expect(local.session).toEqual({ fetchBatchRows: 65536 });
    const prod = ex().createDoc('q3', 'c-prod');
    expect(prod.output.dir).toBe('C:\\exports');
    expect(prod.session.workMem).toBe('64MB');
    expect(prod).toMatchObject({ panel: 'output', panelCollapsed: true, busy: null, sql: '', baseline: '' });
    expect(new Set([before.gen, local.gen, prod.gen]).size).toBe(3);
  });

  it('createDoc drops a BOM seeded alongside EUC-KR (history, CLI --excel --encoding euc-kr)', () => {
    const csv = { header: true, delimiter: ',', nullAs: '', bom: true, crlf: true };
    const euc = ex().createDoc('q1', 'c-prod', { output: { ...defaultOutputOpts(), csv: { ...csv, encoding: 'euc-kr' } } });
    expect(euc.output.csv).toMatchObject({ bom: false, encoding: 'euc-kr' });
    const utf8 = ex().createDoc('q2', 'c-prod', { output: { ...defaultOutputOpts(), csv: { ...csv, encoding: 'utf-8' } } });
    expect(utf8.output.csv).toEqual(csv);
  });

  it('checks with the SQL dialect of the doc connection', async () => {
    const mysqlId = await addMysql();
    ex().createDoc('pg', 'c-prod', { sql: HASH_SQL });
    ex().createDoc('my', mysqlId, { sql: HASH_SQL });
    const pg = await ex().check('pg');
    expect(pg?.issues.map((i) => i.code)).toContain('MULTI_STATEMENT');
    expect(ex().docs.pg.lastCheck).toEqual(pg);
    expect(ex().docs.pg).toMatchObject({ panel: 'check', panelCollapsed: false });
    const my = await ex().check('my');
    expect(my?.ok).toBe(true);
    // Live check: target dialect, busy / lastCheck untouched.
    expect((await ex().checkSql('my', HASH_SQL))?.ok).toBe(true);
    expect((await ex().checkSql('gone', HASH_SQL))).toBeNull();
  });

  it('drops the plan and the check when the text changes, keeps them for the same text', async () => {
    ex().createDoc('q1', 'c-prod', { sql: ORDERS });
    await ex().explain('q1');
    await ex().check('q1');
    expect(ex().docs.q1.lastPlan).not.toBeNull();
    expect(ex().docs.q1.panel).toBe('check');
    ex().setSql('q1', ORDERS);
    expect(ex().docs.q1.lastPlan).not.toBeNull();
    ex().setSql('q1', 'SELECT id FROM public.orders');
    expect(ex().docs.q1.lastPlan).toBeNull();
    expect(ex().docs.q1.lastCheck).toBeNull();
    expect(isDocDirty(ex().docs.q1)).toBe(true);
  });

  it('a plan for text that changed meanwhile is dropped; results stay on their own tab', async () => {
    mockBackend.configure({ latencyMs: 15 });
    ex().createDoc('a', 'c-prod', { sql: ORDERS });
    ex().createDoc('b', 'c-prod', { sql: ORDERS });
    const pa = ex().explain('a');
    expect(ex().docs.a.busy).toBe('explain');
    expect(ex().docs.b.busy).toBeNull();
    await pa;
    expect(ex().docs.a.lastPlan).not.toBeNull();
    expect(ex().docs.b.lastPlan).toBeNull();

    const pb = ex().explain('b');
    ex().setSql('b', 'SELECT id FROM public.orders');
    await pb;
    expect(ex().docs.b.lastPlan).toBeNull();
    expect(ex().docs.b.busy).toBeNull();
  });

  it('close during explain, reopen, then the result arrives: the doc stays idle', async () => {
    mockBackend.configure({ latencyMs: 15 });
    ex().createDoc('q1', 'c-prod', { sql: ORDERS });
    const pending = ex().explain('q1');
    const closed = { ...ex().docs.q1 };
    expect(closed.busy).toBe('explain');
    ex().disposeDoc('q1');
    ex().restoreDoc(closed);
    expect(ex().docs.q1.busy).toBeNull();
    expect(ex().docs.q1.gen).not.toBe(closed.gen);
    await pending;
    expect(ex().docs.q1.busy).toBeNull();
    expect(ex().docs.q1.lastPlan).toBeNull();
  });

  it('runs without warnings straight to export_start and moves the panel to 작업', async () => {
    ex().createDoc('q1', 'c-prod', { sql: ORDERS });
    await ex().run('q1');
    const d = ex().docs.q1;
    expect(d.lastJobId).toMatch(/^job-/);
    expect(d).toMatchObject({ panel: 'job', panelCollapsed: false, busy: null, baseline: ORDERS });
    expect(mockBackend.calls).toEqual(expect.arrayContaining(['export_preflight', 'export_start']));
    expect(ex().preflightDialog).toBeNull();
  });

  it('queues a second tab\'s preflight sheet while one is open; closing shows the next', async () => {
    for (const id of ['a', 'b']) {
      ex().createDoc(id, 'c-prod', { sql: ORDERS });
      ex().setOutput(id, { overwrite: true });
    }
    await Promise.all([ex().run('a'), ex().run('b')]);
    const first = ex().preflightDialog;
    expect(first).toMatchObject({ kind: 'run', tabId: 'a', busy: false, error: null });
    expect(ex().preflightQueue.map((d) => (d.kind === 'run' ? d.tabId : d.jobId))).toEqual(['b']);
    ex().closeDialog();
    expect(ex().preflightDialog).toMatchObject({ kind: 'run', tabId: 'b' });
    expect(ex().preflightQueue).toEqual([]);
    ex().closeDialog();
    expect(ex().preflightDialog).toBeNull();
    expect(mockBackend.calls).not.toContain('export_start');
  });

  it('starts the confirmed request even when its tab closed meanwhile, then opens the next sheet', async () => {
    ex().createDoc('a', 'c-prod', { sql: ORDERS });
    ex().setOutput('a', { overwrite: true });
    await ex().run('a');
    ex().disposeDoc('a');
    const jobId = await ex().startAcknowledged(['OVERWRITE']);
    expect(jobId).toMatch(/^job-/);
    expect(ex().preflightDialog).toBeNull();
  });

  it('a preflight that returns after the doc was closed neither starts nor opens a sheet', async () => {
    mockBackend.configure({ latencyMs: 15 });
    ex().createDoc('a', 'c-prod', { sql: ORDERS });
    ex().setOutput('a', { overwrite: true });
    const pending = ex().run('a');
    ex().disposeDoc('a');
    await pending;
    expect(ex().preflightDialog).toBeNull();
    expect(mockBackend.calls).not.toContain('export_start');
  });

  it('asks for an "ask every time" password before run / explain, with the tab id', async () => {
    const saved = await connSave({ id: '', name: 'ask', driver: 'postgres', host: 'h', user: 'u', fields: {}, session: {}, savePassword: false, hasPassword: false, hasSshSecret: false });
    useConnectionsStore.setState((s) => ({ connections: [...s.connections, saved] }));
    ex().createDoc('q1', saved.id, { sql: ORDERS });
    await ex().run('q1');
    await ex().explain('q1');
    const { useUiStore } = await import('./ui');
    expect(useUiStore.getState().dbPasswordPrompts[0].requests).toMatchObject([
      { reason: 'run', tabId: 'q1' },
      { reason: 'explain', tabId: 'q1' },
    ]);
    expect(mockBackend.calls).not.toContain('export_preflight');
  });

  it('reports an empty document and a check that could not run as errors on the doc', async () => {
    ex().createDoc('q1', 'c-prod');
    expect(ex().buildRequest('q1')).toBeNull();
    expect(ex().docs.q1.error).toBeTruthy();
    ex().clearError('q1');
    expect(ex().docs.q1.error).toBeNull();
    ex().createDoc('q2', 'gone', { sql: 'SELECT 1' });
    expect(await ex().check('q2')).toBeNull();
    expect(ex().docs.q2.error).toMatch(/NOT_FOUND/);
  });

  it('setPanel expands; togglePanel flips; Excel preset sets BOM, CRLF and the row split', () => {
    ex().createDoc('q1', 'c-prod');
    ex().setPanel('q1', 'session');
    expect(ex().docs.q1).toMatchObject({ panel: 'session', panelCollapsed: false });
    ex().togglePanel('q1');
    expect(ex().docs.q1.panelCollapsed).toBe(true);
    ex().applyExcelPreset('q1');
    expect(ex().docs.q1.output).toMatchObject({ format: 'csv', csv: { bom: true, crlf: true }, split: { rows: BigInt(1_000_000) } });
  });
});

describe('extract store: legacy flat API (removed in W3)', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('edits the bound tab doc and mirrors it in the flat fields', () => {
    ex().createDoc('q1', 'c-prod', { sql: 'SELECT 1' });
    ex().bindLegacyDoc('q1');
    expect(ex().sql).toBe('SELECT 1');
    expect(ex().connId).toBe('c-prod');
    ex().setSql('SELECT 2');
    expect(ex().docs.q1.sql).toBe('SELECT 2');
    expect(ex().sql).toBe('SELECT 2');
    // A workspace tab keeps its connection.
    ex().setConnId('c-local');
    expect(ex().docs.q1.connId).toBe('c-prod');
  });

  it('works without a bound tab through a private doc seeded from the flat fields', async () => {
    useExtractStore.setState({ sql: ORDERS, session: { workMem: '64MB' } });
    await ex().run();
    expect(ex().legacyTabId).toBe(LEGACY_DOC_ID);
    expect(ex().docs[LEGACY_DOC_ID].connId).toBe('c-prod');
    await vi.waitFor(() => expect(ex().lastJobId).toMatch(/^job-/));
  });
});
