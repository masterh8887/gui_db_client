// Mock backend: MySQL compatibility fields and the CSV output encoding (docs/mysql-compat.md),
// kept in step with the real registry and the core's checks.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import type { ConnectionInput, DriverInfo, ExportRequest, JobDone, JobSummary, PreflightReport } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { cp949Has, firstNonCp949 } from './cp949';
import {
  catalogColumns,
  catalogDatabases,
  catalogDdl,
  catalogIndexes,
  catalogSchemas,
  catalogSelectSql,
  catalogTables,
  connGet,
  connList,
  connSave,
  connTest,
  driversList,
  exportPreflight,
  exportStart,
  jobGet,
} from './commands';
import { toIpcError } from './ipc-error';
import { listen } from './ipc';
import { LEGACY_CONN_ID, MYSQL8_CONN_ID } from './mock-data';
import { mockBackend } from './mock-ipc';

const FAST = { latencyMs: 0, progressTickMs: 5, prepareMs: 10, runMs: 40, finishingMs: 5, serverIntervalMs: 20, exactCountMs: 20 };

interface RegistryDriver {
  id: string;
  fields: string[];
  defaults: Record<string, string>;
  choices?: Record<string, string[]>;
}

function registryMysql(): RegistryDriver {
  const raw = JSON.parse(readFileSync(resolve(process.cwd(), '../../drivers/registry.json'), 'utf8')) as { drivers: RegistryDriver[] } | RegistryDriver[];
  const list = Array.isArray(raw) ? raw : raw.drivers;
  return list.find((d) => d.id === 'mysql')!;
}

function req(over: Partial<ExportRequest> = {}, csv: Partial<NonNullable<ExportRequest['output']['csv']>> = {}): ExportRequest {
  return {
    connId: LEGACY_CONN_ID,
    sql: 'SELECT * FROM orders',
    session: {},
    output: {
      dir: 'C:\\exports',
      namePattern: '{conn}_{name}',
      format: 'csv',
      csv: { header: true, delimiter: ',', nullAs: '', bom: false, crlf: true, ...csv },
      overwrite: false,
    },
    ...over,
  };
}

async function rejection(p: Promise<unknown>) {
  try {
    await p;
  } catch (e) {
    return toIpcError(e);
  }
  throw new Error('expected a rejection');
}

describe('mock backend: MySQL compatibility and CSV encoding', () => {
  beforeEach(() => {
    mockBackend.reset();
    mockBackend.configure(FAST);
  });

  it('describes the mysql driver exactly like drivers/registry.json (fields, defaults, choices)', async () => {
    const reg = registryMysql();
    const mysql = (await driversList()).find((d: DriverInfo) => d.id === 'mysql')!;
    expect(mysql.fields).toEqual(reg.fields);
    expect(mysql.defaults).toEqual(reg.defaults);
    expect(mysql.choices).toEqual(reg.choices);
  });

  it('lists the legacy demo connection (MySQL 4.0, EUC-KR, testing) and probes it as pre-4.1', async () => {
    const legacy = (await connList()).find((c) => c.id === LEGACY_CONN_ID)!;
    expect(legacy).toMatchObject({ tag: 'testing', driver: 'mysql', fields: { protocol: 'legacy', encoding: 'euc-kr' } });
    const probe = await connTest(LEGACY_CONN_ID);
    expect(probe.server.version).toMatch(/^4\.0\./);
    expect(probe.server.settings.protocol).toBe('legacy (pre-4.1)');
    expect(probe.server.settings.text_plan).toContain('EUC-KR (CP949)');
  });

  it('persists the compatibility fields through conn_save / conn_get', async () => {
    const input: ConnectionInput = {
      id: '',
      name: 'old shop',
      driver: 'mysql',
      host: 'db4.internal',
      port: 3306,
      fields: { sslmode: 'prefer', protocol: 'auto', encoding: 'latin1', allow_old_password: 'true' },
      session: {},
      savePassword: true,
      hasPassword: false,
      hasSshSecret: false,
      password: 'pw',
    };
    const saved = await connSave(input);
    expect((await connGet(saved.id)).fields).toEqual({ sslmode: 'prefer', protocol: 'auto', encoding: 'latin1', allow_old_password: 'true' });
    expect((await connTest(saved.id)).server.settings.protocol).toBe('modern');
  });

  it('rejects an unknown field value with INVALID naming the field, before connecting', async () => {
    const saved = await connSave({
      id: '',
      name: 'typo',
      driver: 'mysql',
      host: 'db.internal',
      fields: { protocol: 'v4', encoding: 'auto' },
      session: {},
      savePassword: true,
      hasPassword: false,
      hasSshSecret: false,
      password: 'pw',
    });
    const e = await rejection(connTest(saved.id));
    expect(e.code).toBe('INVALID');
    expect(e.message).toBe("mysql field protocol: unknown value 'v4' (use auto, modern or legacy)");
    // Aliases are accepted like the core does.
    await connSave({ ...(await connGet(saved.id)), fields: { protocol: '4.0', encoding: 'cp949', allow_old_password: 'yes' } });
    await expect(connTest(saved.id)).resolves.toMatchObject({ server: { settings: { protocol: 'legacy (pre-4.1)' } } });
  });

  it('raises SQL_ENCODING (character + line / column) for SQL outside CP949 on an euc-kr connection', async () => {
    const blocked: PreflightReport = await exportPreflight(req({ sql: "SELECT *\nFROM orders WHERE memo = '😀'" }));
    const issue = blocked.blockers.find((b) => b.code === 'SQL_ENCODING')!;
    expect(issue.severity).toBe('danger');
    expect(issue.message).toContain("'😀' (U+1F600) at line 2, column 27");
    expect(issue.message).toContain('EUC-KR (CP949)');
    expect(issue.detail).toBe('line 2, column 27');
    expect(blocked.warnings.map((w) => w.code)).not.toContain('SQL_ENCODING');
    // Korean text is CP949; a UTF-8 connection never blocks.
    expect((await exportPreflight(req({ sql: "SELECT * FROM orders WHERE memo = '한글'" }))).blockers).toEqual([]);
    expect((await exportPreflight(req({ connId: 'c-prod', sql: "SELECT * FROM public.orders WHERE note = '😀'" }))).blockers.map((b) => b.code)).not.toContain('SQL_ENCODING');
  });

  it('checks CsvOpts.encoding like the core: NULL text outside CP949 blocks, a letter delimiter is INVALID, a BOM is ignored', async () => {
    const r = await exportPreflight(req({}, { encoding: 'euc-kr', nullAs: '🚫' }));
    expect(r.blockers.map((b) => b.code)).toEqual(['CSV_ENCODING']);
    expect((await exportPreflight(req({}, { nullAs: '🚫' }))).blockers).toEqual([]);
    const e = await rejection(exportPreflight(req({}, { encoding: 'euc-kr', delimiter: 'x' })));
    expect(e.code).toBe('INVALID');
    // Core `CsvOpts::writes_bom`: with EUC-KR the BOM is skipped without an error.
    expect((await exportPreflight(req({}, { encoding: 'euc-kr', bom: true }))).blockers).toEqual([]);
  });

  it('keeps csv.encoding in the job request and adds the replaced-characters note to an EUC-KR result', async () => {
    const done: JobDone[] = [];
    await listen<JobDone>('job:done', (d) => done.push(d));
    const acks = ['OVERWRITE', 'CONCURRENT_SESSION', 'WORK_MEM_HIGH'];
    const id = await exportStart(req({}, { encoding: 'euc-kr' }), acks);
    await expect.poll(() => done.find((d) => d.jobId === id), { timeout: 2000 }).toBeDefined();
    const d = done.find((x) => x.jobId === id)!;
    expect(d.state).toBe('done');
    expect(d.result?.notes).toContain("3 character(s) cannot be represented in EUC-KR (CP949) and were written as '?'");
    const job: JobSummary = await jobGet(id);
    expect(job.request?.output.csv?.encoding).toBe('euc-kr');
    // A UTF-8 export is lossless: no such note.
    const id2 = await exportStart(req({ connId: 'c-prod', sql: 'SELECT * FROM public.orders' }), acks);
    await expect.poll(() => done.find((x) => x.jobId === id2), { timeout: 2000 }).toBeDefined();
    expect(done.find((x) => x.jobId === id2)?.result?.notes.join('\n')).not.toContain('cannot be represented');
  });
});

describe('cp949 table', () => {
  it('knows Hangul, KS X 1001 symbols and hanja, and rejects emoji', () => {
    for (const ch of ['a', '한', '똠', '※', '·', '漢']) expect(cp949Has(ch), ch).toBe(true);
    for (const ch of ['😀', '🚫']) expect(cp949Has(ch), ch).toBe(false);
    expect(firstNonCp949('select\n  1 -- 😀')).toEqual({ ch: '😀', line: 2, column: 8 });
    expect(firstNonCp949('select 1')).toBeNull();
  });
});

describe('mock backend: the MySQL catalogs', () => {
  beforeEach(() => {
    mockBackend.reset();
    mockBackend.configure(FAST);
  });

  it('browses the legacy 4.0 server as EUC-KR shop / board with Korean table comments and 4.0 DDL', async () => {
    const dbs = await catalogDatabases(LEGACY_CONN_ID);
    expect(dbs.map((d) => [d.name, d.encoding, d.isCurrent])).toEqual([
      ['shop', 'euckr', true],
      ['board', 'euckr', false],
    ]);
    // MySQL has no schemas: each database is its own (as core lists it).
    expect((await catalogSchemas(LEGACY_CONN_ID, 'shop')).map((s) => s.name)).toEqual(['shop']);
    const tables = await catalogTables(LEGACY_CONN_ID, 'shop', 'shop');
    expect(tables.find((t) => t.name === 'orders')).toMatchObject({ schema: 'shop', comment: '주문' });
    const ref = { database: 'shop', schema: 'shop', name: 'orders' };
    const cols = await catalogColumns(LEGACY_CONN_ID, ref);
    expect(cols.map((c) => c.dataType)).toEqual(['int(11)', 'int(11)', "enum('신규','결제','배송','취소')", 'decimal(12,2)', 'timestamp(14)', 'datetime', 'text']);
    expect(cols.find((c) => c.name === 'shipped_at')?.default).toBe('0000-00-00 00:00:00');
    const ddl = await catalogDdl(LEGACY_CONN_ID, ref);
    expect(ddl).toMatch(/^CREATE TABLE `orders` \(/);
    expect(ddl).toContain('`id` int(11) NOT NULL auto_increment');
    expect(ddl).toContain('PRIMARY KEY  (`id`)');
    expect(ddl).toContain("TYPE=MyISAM COMMENT='주문'");
    expect((await catalogIndexes(LEGACY_CONN_ID, ref)).map((i) => i.name)).toEqual(['PRIMARY', 'idx_orders_customer', 'idx_orders_ordered']);
    expect(await catalogSelectSql(LEGACY_CONN_ID, ref)).toContain('FROM `shop`.`orders`');
  });

  it('seeds a write-allowed MySQL 8 demo target whose app database is InnoDB utf8mb4', async () => {
    const conn = (await connList()).find((c) => c.id === MYSQL8_CONN_ID)!;
    expect(conn).toMatchObject({ driver: 'mysql', host: 'mysql8.internal', writeAllowed: true, tag: 'development' });
    expect((await connTest(MYSQL8_CONN_ID)).server.version).toBe('8.0.36');
    expect((await catalogDatabases(MYSQL8_CONN_ID)).map((d) => [d.name, d.encoding])).toEqual([['app', 'utf8mb4']]);
    const ddl = await catalogDdl(MYSQL8_CONN_ID, { database: 'app', schema: 'app', name: 'members' });
    expect(ddl).toContain("`name` varchar(50) NOT NULL COMMENT '이름'");
    expect(ddl).toContain("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci COMMENT='회원'");
    // Any other MySQL connection browses one of the two servers by its protocol / host.
    const other = await connSave({ id: '', name: 'my', driver: 'mysql', host: 'db.example', database: 'app', fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false, password: 'pw' });
    expect((await catalogDatabases(other.id)).map((d) => d.name)).toEqual(['app']);
  });
});
