// The browser mock of the DB copy commands (lib/mock-copy.ts): realistic plans for the demo
// connections (legacy MySQL 4.0 EUC-KR `shop` → the MySQL 8 demo target, PostgreSQL 15 → 16 with
// the default skip), a timed run with `seq`-ordered `copy:progress` deltas, cancel with what
// stays behind, resume as a new job of the same run, dry runs, plan cancel, the start-time gates
// and warned jobs (missing acknowledgements, a changed plan) continued through `job_resume`.

import type { Connection, CopyPlan, CopyProgress, CopyRequest, CopyStatus } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connGet, connSave, copyPlan, copyPlanCancel, copyResume, copyRun, copyRuns, copyStart, copyStatus, jobCancel, jobGet, jobResume } from '@/lib/commands';
import { listen } from '@/lib/ipc';
import { LEGACY_CONN_ID, MYSQL8_CONN_ID, mockConnections, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { DEFAULT_COPY_OPTIONS } from '@/stores/copy';

function req(source: Partial<CopyRequest['source']>, target: Partial<CopyRequest['target']>, options: Partial<CopyRequest['options']> = {}): CopyRequest {
  return {
    source: { connId: '', database: '', ...source },
    target: { connId: '', database: '', ...target },
    selection: { kind: 'all' },
    options: { ...structuredClone(DEFAULT_COPY_OPTIONS), ...options },
  };
}

async function writable(connId: string): Promise<Connection> {
  const conn = [...mockConnections(), mockLegacyConnection()].find((c) => c.id === connId)!;
  return connSave({ ...conn, writeAllowed: true });
}

/** A MySQL 8.0 target (`mysql8.internal`), saved and marked write-allowed. */
async function mysql8(host = 'mysql8.internal'): Promise<Connection> {
  const created = await connSave({
    id: '',
    name: 'mysql8',
    driver: 'mysql',
    host,
    port: 3306,
    database: 'shop',
    user: 'copy',
    fields: { sslmode: 'prefer' },
    session: {},
    savePassword: true,
    hasPassword: false,
    hasSshSecret: false,
  });
  return connSave({ ...created, writeAllowed: true });
}

async function waitState(jobId: string, states: string[], timeout = 4000): Promise<CopyStatus> {
  let last: CopyStatus | undefined;
  await vi.waitFor(
    async () => {
      last = await copyStatus(jobId);
      expect(states).toContain(last.state);
    },
    { timeout, interval: 10 },
  );
  return last!;
}

const acksOf = (p: CopyPlan) => p.acks.map((a) => a.key);
const codes = (p: CopyPlan) => [...p.warnings, ...p.items.flatMap((i) => i.warnings)].map((i) => i.code);

describe('mock-copy', () => {
  beforeEach(() => {
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, progressTickMs: 4 });
  });

  it('plans the legacy MySQL 4.0 EUC-KR database into a new utf8mb4 database on MySQL 8 with its warnings', async () => {
    // The seeded demo target: MySQL 8.0, write-allowed, tagged development.
    const tgt = await connGet(MYSQL8_CONN_ID);
    expect(tgt).toMatchObject({ driver: 'mysql', writeAllowed: true, tag: 'development' });
    const p = await copyPlan(req({ connId: LEGACY_CONN_ID, database: 'shop' }, { connId: tgt.id, database: 'shop_utf8' }));
    expect(p.family).toBe('mysql');
    expect(p.source.version).toMatch(/^4\.0\./);
    expect(p.source.charset).toBe('euckr');
    expect(p.target.version).toBe('8.0.36');
    expect(p.effective).toMatchObject({ charset: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci', engine: 'InnoDB', zeroDates: 'null', loadMethod: 'mysql_insert', consistentSnapshot: false });
    const db = p.items.find((i) => i.kind === 'database');
    expect(db?.targetSql[0]).toBe('CREATE DATABASE `shop_utf8` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci');
    const orders = p.items.find((i) => i.id === 'table:"orders"')!;
    expect(orders.action).toBe('create');
    expect(orders.sourceDdl).toContain('TYPE=MyISAM');
    expect(orders.sourceDdl).toContain('timestamp(14)');
    expect(orders.targetSql[0]).toContain('ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci');
    expect(orders.targetSql[0]).toContain('timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP');
    expect(orders.targetSql[0]).toContain('`shipped_at` datetime NULL DEFAULT NULL');
    // Korean ENUM values and the table comment travel as UTF-8 text; 4.0 DECIMAL gains a digit.
    expect(orders.sourceDdl).toContain("COMMENT='주문'");
    expect(orders.targetSql[0]).toContain("`status` enum('신규','결제','배송','취소') NOT NULL DEFAULT '신규'");
    expect(orders.targetSql[0]).toContain('`total` decimal(13,2) NOT NULL');
    expect(p.items.filter((i) => i.kind === 'table').map((i) => i.name)).toEqual(['customers', 'orders', 'order_items', 'products', 'categories', 'reviews', 'coupons', 'notices']);
    expect(orders.targetSql.at(-1)).toMatch(/^INSERT INTO `shop_utf8`\.`orders` \(.*\) VALUES <rows>$/);
    expect(codes(p)).toEqual(expect.arrayContaining(['SRC_NOT_SNAPSHOT', 'ZERO_DATE_NULL', 'COLLATION_UNIQUE_RISK', 'LOCAL_INFILE_OFF', 'TIMESTAMP_SEMANTICS', 'DECIMAL_WIDENED']));
    expect(p.blockers.map((b) => b.code)).toEqual(['ENUM_DUPLICATE']);
    expect(p.blockers[0].itemId).toBe('table:"products"');
    // Info issues never need an acknowledgement; warnings keyed by their items do.
    expect(p.acks.map((a) => a.code)).toEqual(expect.arrayContaining(['SRC_NOT_SNAPSHOT', 'ZERO_DATE_NULL', 'COLLATION_UNIQUE_RISK']));
    expect(p.acks.some((a) => a.code === 'LOCAL_INFILE_OFF')).toBe(false);
    expect(p.acks.find((a) => a.code === 'ZERO_DATE_NULL')?.key).toMatch(/^ZERO_DATE_NULL#[0-9a-f]{12}$/);
    expect(p.acks.find((a) => a.code === 'SRC_NOT_SNAPSHOT')?.key).toBe('SRC_NOT_SNAPSHOT');
    // The per-table collation override resolves the ENUM duplicate.
    const fixed = await copyPlan(
      req({ connId: LEGACY_CONN_ID, database: 'shop' }, { connId: tgt.id, database: 'shop_utf8' }, { mysql: { ...DEFAULT_COPY_OPTIONS.mysql, tableOverrides: [{ table: 'products', collation: 'utf8mb4_bin' }] } }),
    );
    expect(fixed.blockers).toEqual([]);
    expect(fixed.items.find((i) => i.id === 'table:"products"')?.targetSql[0]).toContain('COLLATE=utf8mb4_bin');
  });

  it('a 5.7 target with local_infile on loads with LOAD DATA and keeps zero dates by default on 5.5', async () => {
    const t57 = await mysql8('mysql-5.7.internal');
    const p = await copyPlan(req({ connId: LEGACY_CONN_ID, database: 'shop' }, { connId: t57.id, database: 'shop_utf8' }));
    expect(p.effective.loadMethod).toBe('mysql_load_data');
    expect(p.effective.collation).toBe('utf8mb4_general_ci');
    expect(p.items.find((i) => i.id === 'table:"orders"')?.targetSql.at(-1)).toMatch(/^LOAD DATA LOCAL INFILE 'bulkdb-/);
    const t55 = await mysql8('mysql-5.5.internal');
    const p55 = await copyPlan(req({ connId: LEGACY_CONN_ID, database: 'shop' }, { connId: t55.id, database: 'shop_utf8' }));
    expect(p55.effective.zeroDates).toBe('keep');
    expect(codes(p55)).toContain('ZERO_DATE_KEEP');
    expect(codes(p55)).toContain('UNIQUE_PREFIX');
  });

  it('PostgreSQL 15 → 16 into a database that already has the tables skips them by default and needs that acknowledged', async () => {
    await writable('c-staging');
    const r = req({ connId: 'c-prod', database: 'orders', schema: 'public' }, { connId: 'c-staging', database: 'orders' });
    const p = await copyPlan(r);
    expect(p.source.version).toBe('15.8');
    expect(p.target.version).toBe('16.4');
    const tables = p.items.filter((i) => i.kind === 'table');
    expect(tables.length).toBeGreaterThan(5);
    expect(tables.every((t) => t.action === 'skip_existing' && t.existsInTarget)).toBe(true);
    expect(tables.some((t) => t.targetHasRows === false)).toBe(true);
    expect(p.items.filter((i) => i.parent && i.kind === 'index').every((i) => i.action === 'skip_existing')).toBe(true);
    const ack = p.acks.find((a) => a.code === 'TABLES_SKIPPED_EXISTING')!;
    expect(ack.items).toHaveLength(tables.length);
    // Without the acknowledgement the job waits in `warned` (core 5.1), one report warning per
    // missing key; `job_resume` must name every key.
    const warned = await copyStart(r, p.planId, []);
    await waitState(warned, ['warned']);
    expect((await jobGet(warned)).preflight?.warnings.map((w) => w.code)).toEqual(acksOf(p));
    expect((await copyStatus(warned)).plan?.planId).toBe(p.planId);
    await expect(jobResume(warned, [])).rejects.toMatchObject({ code: 'INVALID' });
    expect((await copyStatus(warned)).state).toBe('warned');
    await jobResume(warned, acksOf(p));
    expect((await waitState(warned, ['done'])).result?.skippedExisting).toBe(tables.length);
    // An override for one table changes the action and the acknowledgement key.
    const orders = tables.find((t) => t.name === 'orders')!;
    const p2 = await copyPlan({ ...r, options: { ...r.options, existingOverrides: [{ itemId: orders.id, policy: 'truncate' }] } });
    const o2 = p2.items.find((i) => i.id === orders.id)!;
    expect(o2.action).toBe('truncate');
    expect(o2.targetSql).toEqual(['TRUNCATE ONLY "public"."orders"', expect.stringMatching(/^COPY "public"."orders" \(.*\) FROM STDIN \(FORMAT csv\)$/)]);
    expect(p2.acks.find((a) => a.code === 'TABLES_SKIPPED_EXISTING')?.key).not.toBe(ack.key);
    expect(p2.acks.map((a) => a.code)).toContain('DESTRUCTIVE_TRUNCATE');
  });

  it('a new schema in another database is created with its tables, keys after the data and setval templates', async () => {
    await writable('c-staging');
    const p = await copyPlan(req({ connId: 'c-prod', database: 'orders', schema: 'public' }, { connId: 'c-staging', database: 'analytics_dw', schema: 'shop' }));
    expect(p.items[0]).toMatchObject({ kind: 'schema', name: 'public', targetName: 'shop', action: 'create', targetSql: ['CREATE SCHEMA "shop"'] });
    const orders = p.items.find((i) => i.id === 'table:"public"."orders"')!;
    expect(orders.targetSql[0]).toMatch(/^CREATE TABLE "shop"."orders" \(/);
    expect(p.items.find((i) => i.id === 'constraint:"public"."orders"."orders_pkey"')).toMatchObject({ phase: 'post_data', action: 'create' });
    expect(p.items.find((i) => i.kind === 'sequence_value' && i.parent === orders.id)?.targetSql[0]).toContain('<source value>');
    expect(p.items.find((i) => i.kind === 'foreign_key' && i.parent === 'table:"public"."order_items"')?.dependsOn).toContain(orders.id);
    expect(p.blockers).toEqual([]);
  });

  it('runs a copy with seq-ordered deltas and ends done with problems (legacy value fixes)', async () => {
    const tgt = await mysql8();
    const deltas: CopyProgress[] = [];
    await listen<CopyProgress>('copy:progress', (p) => deltas.push(p));
    const r = req(
      { connId: LEGACY_CONN_ID, database: 'shop' },
      { connId: tgt.id, database: 'shop_utf8' },
      { tableConcurrency: 4, mysql: { ...DEFAULT_COPY_OPTIONS.mysql, tableOverrides: [{ table: 'products', collation: 'utf8mb4_bin' }] } },
    );
    const p = await copyPlan(r);
    const jobId = await copyStart(r, p.planId, acksOf(p));
    expect(mockBackend.getJob(jobId)?.copy).toMatchObject({ targetConnId: tgt.id, targetDatabase: 'shop_utf8', dryRun: false });
    const s = await waitState(jobId, ['done']);
    expect(deltas.length).toBeGreaterThan(3);
    expect(deltas.every((d) => !d.full)).toBe(true);
    const seqs = deltas.map((d) => Number(d.seq));
    expect([...seqs].sort((a, b) => a - b)).toEqual(seqs);
    expect(deltas.flatMap((d) => d.tables).every((t) => Number(t.seq) <= Number(deltas.at(-1)!.seq))).toBe(true);
    const res = s.result!;
    expect(res.problems).toBe(2);
    expect(res.verified).toBe(false);
    const orders = res.tables.find((t) => t.itemId === 'table:"orders"')!;
    expect(orders.problems).toContain('INVALID_DATE');
    expect(orders.verify).toBe('lossy');
    expect(orders.valueFixes.map((f) => f.code)).toEqual(expect.arrayContaining(['ZERO_DATE_NULL', 'INVALID_DATE']));
    expect(res.tables.find((t) => t.itemId === 'table:"customers"')?.unmappableChars).toBe(BigInt(3));
    expect(res.warnings.map((w) => w.code)).toContain('UNMAPPABLE_CHARS');
    expect(res.tables.find((t) => t.itemId === 'table:"categories"')?.verify).toBe('match');
    expect(s.progress.tables.every((t) => t.state === 'done')).toBe(true);
  });

  it('cancel keeps a resumable result; resume is a new job of the same run that skips what was done', async () => {
    mockBackend.configure({ progressTickMs: 15 });
    await writable('c-staging');
    const r = req({ connId: 'c-prod', database: 'orders', schema: 'public' }, { connId: 'c-staging', database: 'analytics_dw' });
    const p = await copyPlan(r);
    const jobId = await copyStart(r, p.planId, acksOf(p));
    await vi.waitFor(async () => expect((await copyStatus(jobId)).progress.tables.some((t) => t.state === 'done')).toBe(true), { timeout: 4000, interval: 10 });
    await jobCancel(jobId);
    const s = await waitState(jobId, ['cancelled']);
    expect(s.result?.resumable).toBe(true);
    expect(s.result?.leftBehind.some((l) => l.kind === 'created_table')).toBe(true);
    expect(s.result?.leftBehind.some((l) => l.kind === 'sequence_not_set')).toBe(true);
    const [run] = await copyRuns('c-staging');
    expect(run).toMatchObject({ runId: jobId, state: 'cancelled', resumable: true, inUse: false });

    mockBackend.configure({ progressTickMs: 4 });
    const resumed = await copyResume({ runId: jobId, acknowledged: [], partial: 'fail' });
    expect(resumed).not.toBe(jobId);
    const s2 = await waitState(resumed, ['done']);
    expect(s2.runId).toBe(jobId);
    expect(s2.plan?.resumeOf).toBe(jobId);
    expect(s2.plan?.items.some((i) => i.action === 'resume_done')).toBe(true);
    expect(s2.plan?.items.some((i) => i.warnings.some((w) => w.code === 'RESUME_RELOAD'))).toBe(true);
    expect((await copyRun(jobId)).info.lastJobId).toBe(resumed);
  });

  it('a production target needs its exact name at start and at resume', async () => {
    await writable('c-prod');
    const r = req({ connId: 'c-staging', database: 'orders', schema: 'public' }, { connId: 'c-prod', database: 'analytics_dw' });
    const p = await copyPlan(r);
    expect(p.warnings.find((w) => w.code === 'PRODUCTION_CONFIRM')?.severity).toBe('info');
    expect(p.blockers).toEqual([]);
    // As in core (5.1), the call returns a job; the start-time plan's blockers end it `rejected`
    // without a result, the reason only in the job's error.
    const missing = await copyStart(r, p.planId, acksOf(p));
    const rejected = await waitState(missing, ['rejected']);
    expect(rejected.result).toBeUndefined();
    expect(rejected.plan?.blockers.map((b) => b.code)).toContain('PRODUCTION_CONFIRM');
    expect((await jobGet(missing)).error).toMatchObject({ code: 'INVALID', message: expect.stringContaining('PRODUCTION_CONFIRM'), detail: expect.stringContaining('PRODUCTION_CONFIRM: ') });
    const wrong = await copyStart({ ...r, options: { ...r.options, confirmProduction: 'prod' } }, p.planId, acksOf(p));
    await waitState(wrong, ['rejected']);
    // The typed name is a start-time field: the same plan id still binds.
    const jobId = await copyStart({ ...r, options: { ...r.options, confirmProduction: 'prod (PostgreSQL)' } }, p.planId, acksOf(p));
    await waitState(jobId, ['done']);
    const resumed = await copyResume({ runId: jobId, acknowledged: [], partial: 'fail' });
    await waitState(resumed, ['rejected']);
    expect((await jobGet(resumed)).error?.message).toContain('PRODUCTION_CONFIRM');
    // The run keeps the result of its last job that had one.
    const detail = await copyRun(jobId);
    expect(detail.info).toMatchObject({ lastJobId: resumed, state: 'rejected', resumable: true });
    expect(detail.result?.runId).toBe(jobId);
  });

  it('jobs start with no table count; it is known once planned and sent on copy:progress', async () => {
    await writable('c-staging');
    const seen: CopyProgress[] = [];
    const off = await listen<CopyProgress>('copy:progress', (p) => seen.push(p));
    const r = req({ connId: 'c-prod', database: 'orders', schema: 'public' }, { connId: 'c-staging', database: 'analytics_dw' });
    const p = await copyPlan(r);
    const jobId = await copyStart(r, p.planId, acksOf(p));
    expect((await jobGet(jobId)).copy?.tablesTotal).toBe(0);
    await waitState(jobId, ['done']);
    off();
    const loaded = p.items.filter((i) => i.kind === 'table' && i.action !== 'excluded' && i.action !== 'unsupported').length;
    expect(loaded).toBeGreaterThan(0);
    const first = seen.find((e) => e.jobId === jobId)!;
    expect(first).toMatchObject({ full: false, tablesTotal: loaded });
    expect((await jobGet(jobId)).copy?.tablesTotal).toBe(loaded);
  });

  it('a dry run plans every table, executes nothing and is never resumable', async () => {
    await writable('c-local');
    const r = req({ connId: 'c-local', database: 'main' }, { connId: 'c-local', database: 'other' }, { dryRun: true });
    const p = await copyPlan({ ...r, options: { ...r.options, dryRun: false } });
    expect(p.planId).toBe((await copyPlan(r)).planId);
    const jobId = await copyStart(r, p.planId, acksOf(p));
    const s = await waitState(jobId, ['done']);
    expect(s.result?.dryRun).toBe(true);
    expect(s.result?.resumable).toBe(false);
    expect(s.result?.tables.every((t) => t.state === 'planned' && t.rowsWritten === BigInt(0))).toBe(true);
    expect((await copyRuns())[0]).toMatchObject({ dryRun: true, resumable: false });
    await expect(copyResume({ runId: jobId, acknowledged: [], partial: 'fail' })).rejects.toMatchObject({ code: 'INVALID' });
  });

  it('copy_plan_cancel stops a plan in flight; a changed plan waits in warned with PLAN_CHANGED', async () => {
    mockBackend.configure({ latencyMs: 20 });
    await writable('c-staging');
    const r = req({ connId: 'c-prod', database: 'orders', schema: 'analytics' }, { connId: 'c-staging', database: 'analytics_dw' });
    const pending = copyPlan(r, 'tok-1');
    await new Promise((res) => setTimeout(res, 60));
    await copyPlanCancel('tok-1');
    await expect(pending).rejects.toMatchObject({ code: 'CANCELLED' });
    const p = await copyPlan(r);
    // Options that change no item keep the plan id; ones that change the statements do not.
    expect((await copyPlan({ ...r, options: { ...r.options, existing: 'append' } })).planId).toBe(p.planId);
    const changed = { ...r, options: { ...r.options, content: 'schema_only' as const } };
    const jobId = await copyStart(changed, p.planId, acksOf(p));
    const s = await waitState(jobId, ['warned']);
    const notice = s.plan!.warnings.find((w) => w.code === 'PLAN_CHANGED')!;
    expect(notice.detail).toMatch(/^changed items: table:"analytics"\."/);
    expect(s.plan!.acks.find((a) => a.code === 'PLAN_CHANGED')?.key).toBe('PLAN_CHANGED');
    expect((await jobGet(jobId)).preflight?.warnings.map((w) => w.code)).toContain('PLAN_CHANGED');
    await jobResume(jobId, s.plan!.acks.map((a) => a.key));
    await waitState(jobId, ['done']);
    // A plan this process never made is named as unknown.
    const other = await copyStart(r, 'f'.repeat(32), acksOf(p));
    const s2 = await waitState(other, ['warned']);
    expect(s2.plan!.warnings.find((w) => w.code === 'PLAN_CHANGED')?.detail).toContain('not known to this process');
    await jobCancel(other);
    await waitState(other, ['cancelled']);
  });

  it('refuses a target that is not write-allowed, the source database itself and another family', async () => {
    const same = await copyPlan(req({ connId: 'c-prod', database: 'orders' }, { connId: 'c-prod', database: 'orders' }));
    expect(same.blockers.map((b) => b.code)).toEqual(expect.arrayContaining(['TARGET_NOT_WRITABLE', 'SAME_DATABASE']));
    await writable('c-local');
    const family = await copyPlan(req({ connId: 'c-prod', database: 'orders' }, { connId: 'c-local', database: 'main' }));
    expect(family.blockers.map((b) => b.code)).toContain('FAMILY_MISMATCH');
    expect(family.items).toEqual([]);
    // A database the fixtures do not know plans two plain tables.
    await writable('c-staging');
    const generic = await copyPlan(req({ connId: 'c-prod', database: 'shop' }, { connId: 'c-staging', database: 'app_copy' }));
    expect(generic.items.filter((i) => i.kind === 'table').map((i) => i.name)).toEqual(['customers', 'orders']);
    expect(generic.blockers).toEqual([]);
  });
});
