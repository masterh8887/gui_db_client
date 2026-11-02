// The copy wizard store (stores/copy.ts, docs/db-copy.md 7 "Stores"): fresh options on every open,
// the generation guard and plan tokens, ack keys across re-plans, the start gate (write-allowed
// target, blockers, acknowledgements, production name), exclude and per-item policies, the
// `seq` merge of progress, dry runs, cancel and resume, and the write-allowed actions.

import type { CopyProgress, CopyStatus, CopyTableProgress } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave, jobCancel } from '@/lib/commands';
import { LEGACY_CONN_ID, MYSQL8_CONN_ID, mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { DEFAULT_COPY_OPTIONS, useCopyStore } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { resetStores } from '@/test/stores';
import { emptyProgress, mergeProgress, mergeStatus, startBlock } from './model';

const store = () => useCopyStore.getState();
const wiz = () => useCopyStore.getState().wizard;

async function allowWrite(connId: string): Promise<void> {
  const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId)!;
  await connSave({ ...conn, writeAllowed: true });
  await useConnectionsStore.getState().load();
}

function openPg(target = { connId: 'c-staging', database: 'orders' }) {
  store().open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target });
}

const ackAll = () => {
  for (const a of wiz().plan!.acks) if (!wiz().acks.includes(a.key)) store().toggleAck(a.key);
};

function table(itemId: string, seq: number, rows: number, state: CopyTableProgress['state'] = 'loading'): CopyTableProgress {
  return { seq: BigInt(seq), itemId, state, rowsRead: BigInt(rows), rowsWritten: BigInt(rows), bytes: BigInt(rows), rateRowsPerSec: 0 };
}

function progress(jobId: string, seq: number, tables: CopyTableProgress[], full = false): CopyProgress {
  return { ...emptyProgress(jobId, jobId), seq: BigInt(seq), phase: 'data', tables, full, tablesTotal: 2 };
}

describe('copy store', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, progressTickMs: 4 });
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()], drivers: mockDrivers(), loaded: true });
  });

  it('open() starts from the defaults: a destructive policy, overrides and the target database are never remembered', () => {
    openPg();
    store().setDraft({ options: { ...wiz().draft.options, existing: 'truncate', existingOverrides: [{ itemId: 'table:"public"."orders"', policy: 'drop_recreate' }], content: 'data_only' } });
    store().setConfirmName('x');
    store().open({ source: { connId: LEGACY_CONN_ID }, target: { connId: 'c-staging' } });
    expect(wiz().draft.options).toEqual(DEFAULT_COPY_OPTIONS);
    expect(wiz().draft.options).not.toBe(DEFAULT_COPY_OPTIONS);
    expect(wiz().draft.options.existing).toBe('skip');
    expect(wiz().draft.source).toEqual({ connId: LEGACY_CONN_ID, database: 'shop' });
    expect(wiz().draft.target).toEqual({ connId: 'c-staging', database: '' });
    expect(wiz()).toMatchObject({ open: true, step: 'endpoints', plan: null, acks: [], confirmName: '', jobId: null });
  });

  it('a draft change clears the plan; typing the production name does not', async () => {
    await allowWrite('c-staging');
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    await store().buildPlan();
    const plan = wiz().plan;
    expect(plan?.planId).toMatch(/^[0-9a-f]{32}$/);
    store().setConfirmName('typed');
    expect(wiz().plan).toBe(plan);
    store().setDraft({ options: { ...wiz().draft.options, verify: 'checksum' } });
    expect(wiz().plan).toBeNull();
  });

  it('keeps a plan only when the draft did not change while it was made (generation guard)', async () => {
    await allowWrite('c-staging');
    mockBackend.configure({ latencyMs: 10 });
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    const pending = store().buildPlan();
    expect(wiz().planBusy).toBe(true);
    store().setDraft({ options: { ...wiz().draft.options, tableConcurrency: 2 } });
    await pending;
    expect(wiz().plan).toBeNull();
    expect(wiz().planBusy).toBe(false);
    await store().buildPlan();
    expect(wiz().plan?.effective.tableConcurrency).toBe(2);
  });

  it('cancelPlan drops the answer and tells core', async () => {
    await allowWrite('c-staging');
    mockBackend.configure({ latencyMs: 10 });
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    const pending = store().buildPlan();
    await new Promise((r) => setTimeout(r, 30));
    await store().cancelPlan();
    await pending;
    expect(wiz()).toMatchObject({ plan: null, planBusy: false, planToken: null, error: null });
    expect(mockBackend.calls).toContain('copy_plan_cancel');
  });

  it('acknowledge a code, change the policy, re-plan: [복사 시작] is disabled again', async () => {
    await allowWrite('c-staging');
    openPg();
    await store().buildPlan();
    const conns = () => useConnectionsStore.getState().connections;
    expect(startBlock(wiz(), conns())).toBe('acks');
    const skipKey = wiz().plan!.acks.find((a) => a.code === 'TABLES_SKIPPED_EXISTING')!.key;
    ackAll();
    expect(startBlock(wiz(), conns())).toBeNull();
    await store().setItemPolicy('table:"public"."orders"', 'truncate');
    expect(wiz().draft.options.existingOverrides).toEqual([{ itemId: 'table:"public"."orders"', policy: 'truncate' }]);
    expect(wiz().plan!.items.find((i) => i.id === 'table:"public"."orders"')?.action).toBe('truncate');
    // The old key is gone (other affected items), the destructive one is new.
    expect(wiz().acks).not.toContain(skipKey);
    expect(startBlock(wiz(), conns())).toBe('acks');
    expect(await store().start()).toBeNull();
    // Back to the default policy for that table.
    await store().setItemPolicy('table:"public"."orders"', null);
    expect(wiz().draft.options.existingOverrides).toEqual([]);
  });

  it('a key that stays the same across a re-plan stays acknowledged', async () => {
    await allowWrite('c-staging');
    openPg();
    await store().buildPlan();
    ackAll();
    const before = [...wiz().acks];
    await store().buildPlan();
    expect(wiz().acks.sort()).toEqual(before.sort());
  });

  it('nothing starts without a write-allowed target, even when the access was cleared after planning', async () => {
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    await store().buildPlan();
    expect(wiz().plan!.blockers.map((b) => b.code)).toContain('TARGET_NOT_WRITABLE');
    expect(await store().start()).toBeNull();
    expect(mockBackend.calls).not.toContain('copy_start');

    await allowWrite('c-staging');
    await store().buildPlan();
    expect(wiz().plan!.blockers).toEqual([]);
    ackAll();
    // Another window clears the write access; this window's list still says write-allowed.
    const conn = useConnectionsStore.getState().connections.find((c) => c.id === 'c-staging')!;
    await connSave({ ...conn, writeAllowed: false });
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-staging')?.writeAllowed).toBe(true);
    expect(await store().start()).toBeNull();
    expect(wiz().error).toBeTruthy();
    expect(mockBackend.calls).not.toContain('copy_start');
  });

  it('a production target starts only with its exact name, merged at start and never kept in the draft', async () => {
    await allowWrite('c-prod');
    store().open({ source: { connId: 'c-staging', database: 'orders', schema: 'analytics' }, target: { connId: 'c-prod', database: 'analytics_dw' } });
    await store().buildPlan();
    ackAll();
    const conns = useConnectionsStore.getState().connections;
    expect(startBlock(wiz(), conns)).toBe('production');
    expect(await store().start()).toBeNull();
    store().setConfirmName('prod');
    expect(await store().start()).toBeNull();
    expect(mockBackend.calls).not.toContain('copy_start');
    store().setConfirmName('prod (PostgreSQL)');
    const jobId = await store().start();
    expect(jobId).toBeTruthy();
    expect(wiz().draft.options.confirmProduction).toBeUndefined();
    expect(wiz().draft.options.dryRun).toBe(false);
    expect(wiz()).toMatchObject({ jobId, step: 'progress', confirmName: '' });
    expect(useJobsStore.getState().local[jobId!]).toBe(true);
  });

  it('exclude() adds the item to options.exclude and re-plans', async () => {
    const tgt = await connSave({ ...mockLegacyConnection(), id: '', name: 'mysql8', host: 'mysql8.internal', fields: { sslmode: 'prefer' } });
    await connSave({ ...tgt, writeAllowed: true });
    await useConnectionsStore.getState().load();
    store().open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await store().buildPlan();
    expect(wiz().plan!.blockers.map((b) => b.code)).toEqual(['ENUM_DUPLICATE']);
    await store().exclude('table:"products"');
    expect(wiz().draft.options.exclude).toEqual(['table:"products"']);
    expect(wiz().plan!.blockers).toEqual([]);
    expect(wiz().plan!.items.find((i) => i.id === 'table:"products"')?.action).toBe('excluded');
    // A per-table collation is the other way out.
    store().setDraft({ options: { ...wiz().draft.options, exclude: [] } });
    await store().setTableCollation('products', 'utf8mb4_bin');
    expect(wiz().plan!.blockers).toEqual([]);
    expect(wiz().draft.options.mysql.tableOverrides).toEqual([{ table: 'products', collation: 'utf8mb4_bin' }]);
  });

  it('start marks the job local and claims it for this window (window_claim_job)', async () => {
    await allowWrite('c-staging');
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    await store().buildPlan();
    ackAll();
    const jobId = (await store().start())!;
    expect(jobId).toBeTruthy();
    expect(useJobsStore.getState().local[jobId]).toBe(true);
    expect(mockBackend.calls).toContain('window_claim_job');
  });

  it('merges progress by seq: older entries never overwrite newer ones, full statuses fill gaps', () => {
    const jobId = 'job-1';
    useCopyStore.setState((s) => ({ wizard: { ...s.wizard, jobId } }));
    store().applyProgress(progress(jobId, 3, [table('a', 3, 300), table('b', 2, 50)]));
    store().applyProgress(progress(jobId, 2, [table('a', 2, 200)]));
    expect(store().runs[jobId].progress.tables.find((t) => t.itemId === 'a')?.rowsWritten).toBe(BigInt(300));
    expect(Number(store().runs[jobId].progress.seq)).toBe(3);
    store().applyProgress(progress(jobId, 5, [table('b', 5, 90, 'done')]));
    expect(store().runs[jobId].progress.tables.map((t) => [t.itemId, Number(t.rowsWritten)])).toEqual([
      ['a', 300],
      ['b', 90],
    ]);
    // A late full status (seq 4) keeps the newer b and adds c.
    const late: CopyStatus = { jobId, runId: jobId, state: 'running', progress: progress(jobId, 4, [table('a', 4, 400), table('b', 4, 70), table('c', 4, 1)], true) };
    const merged = mergeStatus(store().runs[jobId], late);
    expect(merged.progress.tables.map((t) => [t.itemId, Number(t.rowsWritten)])).toEqual([
      ['a', 400],
      ['b', 90],
      ['c', 1],
    ]);
    expect(Number(merged.progress.seq)).toBe(5);
    // Terminal states are final.
    expect(mergeStatus({ ...merged, state: 'done' }, { ...late, state: 'running' }).state).toBe('done');
    expect(mergeProgress(merged.progress, progress(jobId, 6, [])).phase).toBe('data');
    // A job this window does not show gets no status, but its table counts are kept (Activity).
    store().applyProgress(progress('other', 1, [table('x', 1, 1)]));
    expect(store().runs.other).toBeUndefined();
    expect(store().live.other).toEqual({ seq: 1, tablesTotal: 2, tablesDone: 0, failed: [] });
  });

  it('keeps the table counts of every copy job: newest seq wins, failed or cancelled tables stay counted', () => {
    const counts = (seq: number, done: number, tables: CopyTableProgress[]) => ({ ...progress('j', seq, tables), tablesTotal: 5, tablesDone: done });
    store().applyProgress(counts(2, 1, [table('a', 2, 10, 'failed'), table('b', 2, 5)]));
    const first = store().live.j;
    // An identical event changes nothing (no re-render of the Activity rows).
    store().applyProgress(counts(2, 1, [table('b', 2, 5)]));
    expect(store().live.j).toBe(first);
    store().applyProgress(counts(4, 3, [table('c', 4, 1, 'cancelled'), table('a', 4, 10, 'failed')]));
    store().applyProgress(counts(3, 2, []));
    expect(store().live.j).toEqual({ seq: 4, tablesTotal: 5, tablesDone: 3, failed: ['a', 'c'] });
    expect(store().runs.j).toBeUndefined();
  });

  it('start({ dryRun }) sends a dry run; [실제로 복사] re-plans without it', async () => {
    await allowWrite('c-local');
    store().open({ source: { connId: 'c-local' }, target: { connId: 'c-local', database: 'copy' } });
    await store().buildPlan();
    ackAll();
    const jobId = await store().start({ dryRun: true });
    expect(jobId).toBeTruthy();
    expect(wiz().draft.options.dryRun).toBe(false);
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId!))?.result?.dryRun).toBe(true), { timeout: 3000, interval: 10 });
    expect(wiz().step).toBe('result');
    await store().editAgain({ replan: true });
    expect(wiz()).toMatchObject({ jobId: null, step: 'plan' });
    expect(wiz().plan?.planId).toBeTruthy();
  });

  it('follows a job to its result; cancel and resume continue the same run in a new job', async () => {
    mockBackend.configure({ progressTickMs: 15 });
    await allowWrite('c-staging');
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    await store().buildPlan();
    ackAll();
    const jobId = (await store().start())!;
    expect(store().runs[jobId]).toBeDefined();
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId))?.state).toBe('running'), { timeout: 3000, interval: 10 });
    await jobCancel(jobId);
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId))?.state).toBe('cancelled'), { timeout: 3000, interval: 10 });
    expect(wiz().step).toBe('result');
    expect(store().runs[jobId].result?.resumable).toBe(true);
    mockBackend.configure({ progressTickMs: 4 });
    const resumed = await store().resume(store().runs[jobId].runId);
    expect(resumed).toBeTruthy();
    expect(resumed).not.toBe(jobId);
    expect(wiz()).toMatchObject({ jobId: resumed, step: 'progress' });
    await vi.waitFor(async () => expect((await store().refreshStatus(resumed!))?.state).toBe('done'), { timeout: 3000, interval: 10 });
    expect(store().runs[resumed!].runId).toBe(jobId);
    expect(wiz().step).toBe('result');
  });

  it('showRun / showJob reopen a stored run with its request as the draft', async () => {
    await allowWrite('c-staging');
    openPg({ connId: 'c-staging', database: 'analytics_dw' });
    await store().buildPlan();
    ackAll();
    const jobId = (await store().start())!;
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId))?.state).toBe('done'), { timeout: 3000, interval: 10 });
    store().close();
    store().open();
    await store().showRun(jobId);
    expect(wiz()).toMatchObject({ open: true, jobId, step: 'result' });
    expect(wiz().draft.target).toEqual({ connId: 'c-staging', database: 'analytics_dw' });
    expect(wiz().draft.source.schema).toBe('public');
  });

  it('resume never carries acknowledgements of another plan the wizard holds', async () => {
    // Legacy MySQL 4.0 → the MySQL 8 demo target: SRC_NOT_SNAPSHOT (MyISAM) needs an
    // acknowledgement in every plan of the run, resumes included.
    await useConnectionsStore.getState().load();
    store().open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: MYSQL8_CONN_ID, database: 'shop_utf8' } });
    const { options } = wiz().draft;
    store().setDraft({ options: { ...options, mysql: { ...options.mysql, tableOverrides: [{ table: 'products', collation: 'utf8mb4_bin' }] } } });
    await store().buildPlan();
    expect(wiz().plan!.blockers).toEqual([]);
    expect(wiz().plan!.acks.map((a) => a.code)).toContain('SRC_NOT_SNAPSHOT');
    ackAll();
    const keys = [...wiz().acks];
    const jobId = (await store().start())!;
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId))?.state).toBe('done'), { timeout: 6000, interval: 10 });
    // The wizard now edits another copy whose acknowledgements happen to use the same keys.
    store().open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: MYSQL8_CONN_ID, database: 'shop_other' } });
    useCopyStore.setState((s) => ({ wizard: { ...s.wizard, acks: keys } }));
    const resumed = (await store().resume(jobId))!;
    expect(resumed).toBeTruthy();
    // Nothing was pre-acknowledged: the resumed job asks in `warned`, and the wizard shows its plan.
    await vi.waitFor(async () => expect((await store().refreshStatus(resumed))?.state).toBe('warned'), { timeout: 3000, interval: 10 });
    expect(wiz()).toMatchObject({ jobId: resumed, step: 'plan', acks: [] });
  });

  it('a resume rejected at start shows its blockers; the stored result of the run stays reachable', async () => {
    await allowWrite('c-prod');
    const prod = useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')!;
    store().open({ source: { connId: 'c-staging', database: 'orders', schema: 'analytics' }, target: { connId: 'c-prod', database: 'analytics_dw' } });
    await store().buildPlan();
    ackAll();
    store().setConfirmName(prod.name);
    const jobId = (await store().start())!;
    await vi.waitFor(async () => expect((await store().refreshStatus(jobId))?.state).toBe('done'), { timeout: 3000, interval: 10 });
    // Resumed without the production name: core rejects it at its start-time plan.
    const resumed = (await store().resume(jobId))!;
    await vi.waitFor(async () => expect((await store().refreshStatus(resumed))?.state).toBe('rejected'), { timeout: 3000, interval: 10 });
    expect(wiz()).toMatchObject({ jobId: resumed, step: 'plan' });
    expect(wiz().plan?.blockers.map((b) => b.code)).toContain('PRODUCTION_CONFIRM');
    expect(store().runs[resumed].result).toBeUndefined();
    // 최근 복사 → 이어서 실행 / 결과 보기: the run's stored result (with its resume panel), not the empty job.
    await store().showRun(jobId);
    expect(wiz()).toMatchObject({ jobId: resumed, step: 'result', acks: [] });
    expect(store().runs[resumed].result?.runId).toBe(jobId);
  });

  it('markWriteAllowed / clearWriteAllowed save the stored connection and reload the list', async () => {
    expect(await store().markWriteAllowed('c-staging')).toBe(true);
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-staging')?.writeAllowed).toBe(true);
    expect(await store().clearWriteAllowed('c-staging')).toBe(true);
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-staging')?.writeAllowed).toBeUndefined();
    expect(await store().markWriteAllowed('nope')).toBe(false);
    expect(wiz().error).toBeTruthy();
  });
});
