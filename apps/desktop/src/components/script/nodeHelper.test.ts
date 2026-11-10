// @vitest-environment node

// Runs `crates/core/src/node_helper.js` the way the shell does (`node --require helper script.mjs`)
// and checks what a script sees: several connections by name, read-only SQL, the SQL the builder
// sends to each driver, and that the process ends by itself once the script is done.
//
// SQLite runs for real (node:sqlite). PostgreSQL, MySQL and SQL Server use stand-in driver
// modules that record what they are asked and keep the event loop alive like a socket would.

import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { inflateRawSync } from 'node:zlib';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

/**
 * Read the entries of a ZIP written by node_helper's writer by walking the local file headers
 * (it stores sizes in each header and never uses data descriptors), so the .xlsx can be checked
 * without any xlsx library.
 */
function readZip(buf: Buffer): Map<string, string> {
  const out = new Map<string, string>();
  let i = 0;
  while (i + 4 <= buf.length && buf.readUInt32LE(i) === 0x04034b50) {
    const compSize = buf.readUInt32LE(i + 18);
    const nameLen = buf.readUInt16LE(i + 26);
    const extraLen = buf.readUInt16LE(i + 28);
    const name = buf.toString('utf8', i + 30, i + 30 + nameLen);
    const dataStart = i + 30 + nameLen + extraLen;
    const comp = buf.subarray(dataStart, dataStart + compSize);
    out.set(name, inflateRawSync(comp).toString('utf8'));
    i = dataStart + compSize;
  }
  return out;
}

const HELPER = resolve(process.cwd(), '../../crates/core/src/node_helper.js');
const [major, minor] = process.versions.node.split('.').map(Number);
// node:sqlite without a flag, and top-level await in a .js file.
const runnable = major > 22 || (major === 22 && minor >= 13);

interface Conn {
  id: string;
  name: string;
  driver: string;
  granted?: boolean;
  [key: string]: unknown;
}

const granted = (id: string, name: string, driver: string, extra: Record<string, unknown> = {}): Conn => ({
  id,
  name,
  driver,
  host: 'db.internal',
  port: null,
  database: 'main',
  user: 'app',
  password: 'secret',
  fields: {},
  ssh: false,
  granted: true,
  ...extra,
});

let dir = '';
let scripts = 0;

/** Run `source` as a script tab would; the first connection is the tab's own. */
function run(source: string, conns: Conn[]) {
  const file = join(dir, `script-${++scripts}.mjs`);
  writeFileSync(file, source);
  const result = spawnSync(process.execPath, ['--require', HELPER, file], {
    cwd: dir,
    encoding: 'utf8',
    timeout: 20_000,
    env: {
      ...process.env,
      NODE_PATH: join(dir, 'node_modules'),
      BULKDB_CONN_ID: conns[0].id,
      BULKDB_CONN_NAME: conns[0].name,
      BULKDB_DRIVER: conns[0].driver,
      BULKDB_CONNECTIONS: JSON.stringify(conns),
    },
  });
  return { status: result.status, signal: result.signal, stderr: result.stderr, lines: result.stdout.trim().split('\n').filter(Boolean) };
}

/** A script that prints one JSON line per expression; a rejection prints its message. */
const printing = (...expressions: string[]) =>
  expressions.map((e) => `try { console.log(JSON.stringify(await (${e}))); } catch (e) { console.log(JSON.stringify('ERR ' + e.message)); }`).join('\n');

const STUBS: Record<string, string> = {
  // Every stub holds an interval while "connected": only close() lets the process exit.
  pg: `
    exports.Client = class {
      constructor(config) { this.config = config; }
      on() {}
      async connect() { this.timer = setInterval(() => {}, 1000); }
      async query(q) {
        if (typeof q === 'string') { this.setup = q; return { rows: [] }; }
        return { rows: [{ database: this.config.database, setup: this.setup, text: q.text, values: q.values, mode: q.queryMode }] };
      }
      async end() { clearInterval(this.timer); }
    };`,
  'mysql2/promise': `
    exports.createConnection = async (config) => {
      const timer = setInterval(() => {}, 1000);
      let setup = '';
      return {
        on() {},
        async query(sql) { setup = sql; return [[], []]; },
        async execute(sql, values) { return [[{ database: config.database, setup, sql, values }], []]; },
        async end() { clearInterval(timer); },
      };
    };`,
  mssql: `
    exports.ConnectionPool = class {
      constructor(config) { this.config = config; }
      on() {}
      async connect() { this.timer = setInterval(() => {}, 1000); }
      request() {
        const inputs = {};
        const config = this.config;
        return { input(name, value) { inputs[name] = value; }, async query(sql) { return { recordset: [{ database: config.database, timeout: config.requestTimeout, sql, inputs }] }; } };
      }
      async close() { clearInterval(this.timer); }
    };`,
};

describe.skipIf(!runnable)('node_helper.js', () => {
  beforeAll(() => {
    dir = mkdtempSync(join(tmpdir(), 'bulkdb-helper-'));
    for (const [name, source] of Object.entries(STUBS)) {
      const file = join(dir, 'node_modules', `${name}.js`);
      mkdirSync(resolve(file, '..'), { recursive: true });
      writeFileSync(file, source);
    }
    const setup = `
      const { DatabaseSync } = require('node:sqlite');
      const a = new DatabaseSync('kpia1.db');
      a.exec("create table users(id integer primary key, name text, status text, left_at text); insert into users values (1,'kim','active',null),(2,'lee','active','2026-01-01'),(3,'park','idle',null)");
      a.close();
      const b = new DatabaseSync('kpia2.db');
      b.exec('create table orders(id integer primary key, user_id int, total int); insert into orders values (10,1,500),(11,1,700),(12,3,50),(13,9,1)');
      b.close();`;
    writeFileSync(join(dir, 'setup.cjs'), setup);
    const made = spawnSync(process.execPath, [join(dir, 'setup.cjs')], { cwd: dir, encoding: 'utf8' });
    expect(made.status, made.stderr).toBe(0);
  });

  afterAll(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  const sqlite = (id: string, name: string): Conn => granted(id, name, 'sqlite', { host: null, database: null, user: null, password: null, fields: { path: `${name}.db` } });
  const twoDbs = () => [sqlite('c1', 'kpia1'), sqlite('c2', 'kpia2'), { id: 'c3', name: 'prod', driver: 'postgres' }];

  it('opens several connections by name and combines their rows', () => {
    const out = run(
      `const fs = require('fs');
       const a = bulkdb.db('kpia1');
       const b = bulkdb.db('kpia2');
       const users = await a.from('users').select('id', 'name').where({ status: 'active' }).orderBy('id', 'desc').fetch();
       const orders = await b.from('orders').where({ user_id: users.map((u) => u.id) }).orderBy('id');
       fs.writeFileSync('joined.json', JSON.stringify(orders));
       console.log(JSON.stringify({ names: [a.name, b.name, bulkdb.name], users, orders, same: bulkdb.db('kpia1') === a, plain: Object.getPrototypeOf(users[0]) === Object.prototype }));
       console.log(JSON.stringify(bulkdb.connections));
       console.log(JSON.stringify(process.env.BULKDB_CONNECTIONS === undefined));`,
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    expect(JSON.parse(out.lines[0])).toEqual({
      names: ['kpia1', 'kpia2', 'kpia1'],
      users: [
        { id: 2, name: 'lee' },
        { id: 1, name: 'kim' },
      ],
      orders: [
        { id: 10, user_id: 1, total: 500 },
        { id: 11, user_id: 1, total: 700 },
      ],
      same: true,
      plain: true,
    });
    expect(JSON.parse(out.lines[1])).toEqual([
      { name: 'kpia1', driver: 'sqlite', available: true },
      { name: 'kpia2', driver: 'sqlite', available: true },
      { name: 'prod', driver: 'postgres', available: false },
    ]);
    // The password list does not reach processes the script starts.
    expect(out.lines[2]).toBe('true');
  });

  it('builds null, empty-list, limit, offset, count and first-row queries', () => {
    const out = run(
      printing(
        `bulkdb.from('users').where({ left_at: null }).count()`,
        `bulkdb.db('kpia2').from('orders').where({ user_id: [] }).count()`,
        `bulkdb.db('kpia2').from('orders').select('id').orderBy('id').limit(2).offset(1).fetch()`,
        `bulkdb.db('kpia2').from('orders').select('id').orderBy('id').offset(3).fetch()`,
        `bulkdb.db('kpia2').from('orders').orderBy('total', 'desc').fetchOne()`,
        `bulkdb.db('kpia2').from('orders').where({ id: 999 }).fetchOne()`,
        `bulkdb.query('SELECT COUNT(*) AS n FROM users WHERE id > ?', [1])`,
        `bulkdb.from('users').limit('1; x')`,
      ),
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    expect(out.lines.map((l) => JSON.parse(l))).toEqual([
      2,
      0,
      [{ id: 11 }, { id: 12 }],
      [{ id: 13 }],
      { id: 11, user_id: 1, total: 700 },
      null,
      [{ n: 2 }],
      'ERR limit 은(는) 0 이상의 정수여야 합니다.',
    ]);
  });

  it('runs only single read-only statements', () => {
    const blocked = [
      `DELETE FROM users`,
      `UPDATE users SET name = 'x'`,
      `SELECT 1; DELETE FROM users`,
      `WITH gone AS (DELETE FROM users RETURNING *) SELECT * FROM gone`,
      `SELECT * INTO copy FROM users`,
      `/* note */ INSERT INTO users VALUES (9, 'x', 'y', null)`,
      // A lone carriage return ends a comment on some servers.
      `SELECT 1 -- x\rDROP TABLE users`,
    ];
    const allowed = [
      `SELECT name FROM users WHERE name <> 'please update; delete' ORDER BY id`,
      `WITH active AS (SELECT * FROM users WHERE status = 'active') SELECT count(*) AS n FROM active; -- done`,
      `SELECT "name" AS "delete", replace(name, 'k', 'K') AS updated_at FROM users WHERE id = 1`,
    ];
    const out = run(
      printing(...[...blocked, ...allowed].map((sql) => `bulkdb.query(${JSON.stringify(sql)})`), `bulkdb.from('users; DROP TABLE users').fetch()`, `bulkdb.from('users').count()`),
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    const results = out.lines.map((l) => JSON.parse(l));
    for (const [i, sql] of blocked.entries()) expect(results[i], sql).toMatch(/^ERR \[kpia1\] 읽기 전용입니다/);
    expect(results.slice(blocked.length, blocked.length + allowed.length)).toEqual([
      [{ name: 'kim' }, { name: 'lee' }, { name: 'park' }],
      [{ n: 2 }],
      [{ delete: 'kim', updated_at: 'Kim' }],
    ]);
    expect(results[results.length - 2]).toMatch(/^ERR \[kpia1\] 읽기 전용입니다/);
    // Nothing above changed the data.
    expect(results[results.length - 1]).toBe(3);
  });

  it('exports arbitrary JS-built rows to CSV and multi-sheet XLSX', () => {
    const out = run(
      `const users = [
         { id: 1, name: 'kim, "K"', note: 'a\\nb', score: 9.5, active: true, when: new Date('2026-01-02T03:04:05Z') },
         { id: 2, name: '이훈화', note: null, score: 0, active: false, when: null },
       ];
       const summary = [{ metric: '합계', value: 1000000 }];
       console.log(JSON.stringify(await bulkdb.toCsv(users, 'u.csv')));
       console.log(JSON.stringify(await bulkdb.toXlsx({ 사용자: users, 합계: summary }, 'book.xlsx')));
       console.log(JSON.stringify(await bulkdb.toXlsx(users, 'cols.xlsx', { columns: ['name', 'score', 'missing'] })));`,
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    const [csvMeta, bookMeta, colsMeta] = out.lines.map((l) => JSON.parse(l));
    expect(csvMeta).toEqual({ file: 'u.csv', rows: 2, columns: ['id', 'name', 'note', 'score', 'active', 'when'] });
    expect(bookMeta.sheets).toEqual([
      { name: '사용자', rows: 2, columns: ['id', 'name', 'note', 'score', 'active', 'when'] },
      { name: '합계', rows: 1, columns: ['metric', 'value'] },
    ]);

    // CSV: BOM, RFC-4180 quoting, Korean UTF-8, null -> empty, number, bool, ISO date.
    const csv = readFileSync(join(dir, 'u.csv'), 'utf8');
    expect(csv).toBe(
      '﻿' +
        'id,name,note,score,active,when\r\n' +
        '1,"kim, ""K""","a\nb",9.5,true,2026-01-02T03:04:05.000Z\r\n' +
        '2,이훈화,,0,false,\r\n',
    );

    // XLSX: a real ZIP whose parts and cells are what we wrote (validated without an xlsx lib).
    const zip = readZip(readFileSync(join(dir, 'book.xlsx')));
    expect([...zip.keys()]).toEqual(['[Content_Types].xml', '_rels/.rels', 'xl/workbook.xml', 'xl/_rels/workbook.xml.rels', 'xl/worksheets/sheet1.xml', 'xl/worksheets/sheet2.xml']);
    expect(zip.get('xl/workbook.xml')).toContain('<sheet name="사용자" sheetId="1" r:id="rId1"/>');
    expect(zip.get('xl/workbook.xml')).toContain('<sheet name="합계" sheetId="2" r:id="rId2"/>');
    const sheet1 = zip.get('xl/worksheets/sheet1.xml')!;
    expect(sheet1).toContain('<c r="A1" t="inlineStr"><is><t xml:space="preserve">id</t></is></c>');
    expect(sheet1).toContain('<c r="A2"><v>1</v></c>'); // number, not a string
    expect(sheet1).toContain('<t xml:space="preserve">kim, "K"</t>'); // quotes are literal in element text
    expect(sheet1).toContain('<t xml:space="preserve">이훈화</t>');
    expect(sheet1).toContain('<c r="C3"/>'); // null -> empty cell
    // Explicit columns, incl. one absent from every row -> blank cells under it.
    const colsSheet = readZip(readFileSync(join(dir, 'cols.xlsx'))).get('xl/worksheets/sheet1.xml')!;
    expect(colsSheet).toContain('<t xml:space="preserve">missing</t>');
    expect(colsMeta.sheets[0].columns).toEqual(['name', 'score', 'missing']);
  });

  it('explains names it cannot open', () => {
    const conns = [...twoDbs(), granted('c4', 'twin', 'sqlite'), granted('c5', 'twin', 'sqlite'), granted('c6', 'tunnel', 'postgres', { ssh: true }), granted('c7', 'ora', 'oracle')];
    const out = run(printing(`bulkdb.db('prod')`, `bulkdb.db('nope')`, `bulkdb.db('twin')`, `bulkdb.db('tunnel').query('SELECT 1')`, `bulkdb.db('ora').query('SELECT 1')`), conns);
    expect(out.status, out.stderr).toBe(0);
    const [notGranted, unknown, ambiguous, tunnel, driver] = out.lines.map((l) => JSON.parse(l) as string);
    expect(notGranted).toContain('bulkdb.db("prod")');
    expect(unknown).toContain('저장된 연결: kpia1, kpia2, prod');
    expect(ambiguous).toContain('c4, c5');
    expect(tunnel).toMatch(/^ERR \[tunnel\] SSH/);
    expect(driver).toMatch(/^ERR \[ora\] "oracle"/);
  });

  it('speaks each server dialect, makes sessions read-only, and lets the script end', () => {
    const conns = [
      granted('p1', 'pgA', 'postgres', { fields: { sslmode: 'require' } }),
      granted('m1', 'myA', 'mysql'),
      granted('s1', 'msA', 'mssql'),
      granted('m2', 'old', 'mysql', { fields: { protocol: 'legacy' } }),
    ];
    const where = `where({ id: [1, 2], gone: null, kind: 'x' })`;
    const out = run(
      printing(
        `bulkdb.from('t').${where}.limit(5).fetch()`,
        `bulkdb.db('pgA').use('other').query('SELECT 1')`,
        `bulkdb.db('myA').from('t').${where}.offset(3).fetch()`,
        `bulkdb.db('msA').from('t').${where}.limit(5).fetch()`,
        `bulkdb.db('msA', { database: 'other' }).from('t').orderBy('id').limit(5).offset(10).fetch()`,
        `bulkdb.db('msA').from('t').offset(10).fetch()`,
        `bulkdb.db('old').query('SELECT 1')`,
        `bulkdb.query("(SELECT 1) UNION (SELECT $$it's; delete$$)")`,
        `bulkdb.db('myA').query("SELECT 'a\\\\' /*! , (DELETE) */")`,
      ),
      conns,
    );
    // The stand-in drivers stay "connected" until closed: finishing at all proves the helper closed them.
    expect(out.signal, out.stderr).toBeNull();
    expect(out.status, out.stderr).toBe(0);
    const [pg, pgOther, mysql, top, page, pageUnordered, legacy, dollarQuoted, executableComment] = out.lines.map((l) => JSON.parse(l));
    expect(pg).toEqual([
      { database: 'main', setup: 'SET default_transaction_read_only = on', text: 'SELECT * FROM t WHERE id IN ($1, $2) AND gone IS NULL AND kind = $3 LIMIT 5', values: [1, 2, 'x'], mode: 'extended' },
    ]);
    expect(pgOther[0]).toMatchObject({ database: 'other', text: 'SELECT 1', values: [] });
    expect(mysql).toEqual([
      { database: 'main', setup: 'SET SESSION TRANSACTION READ ONLY', sql: 'SELECT * FROM t WHERE id IN (?, ?) AND gone IS NULL AND kind = ? LIMIT 18446744073709551615 OFFSET 3', values: [1, 2, 'x'] },
    ]);
    expect(top).toEqual([{ database: 'main', timeout: 0, sql: 'SELECT TOP (5) * FROM t WHERE id IN (@p0, @p1) AND gone IS NULL AND kind = @p2', inputs: { p0: 1, p1: 2, p2: 'x' } }]);
    expect(page[0]).toMatchObject({ database: 'other', sql: 'SELECT * FROM t ORDER BY id ASC OFFSET 10 ROWS FETCH NEXT 5 ROWS ONLY' });
    expect(pageUnordered[0].sql).toBe('SELECT * FROM t ORDER BY (SELECT NULL) OFFSET 10 ROWS');
    expect(legacy).toMatch(/^ERR \[old\] MySQL 4\.0/);
    // PostgreSQL dollar quoting is a string; a MySQL executable comment is SQL.
    expect(dollarQuoted[0].text).toBe("(SELECT 1) UNION (SELECT $$it's; delete$$)");
    expect(executableComment).toMatch(/^ERR \[myA\] MySQL 실행 주석/);
  });
});
