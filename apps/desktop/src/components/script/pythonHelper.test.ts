// @vitest-environment node

// Runs `crates/core/src/python_helper.py` the way the shell does (`python <helper> <script.py>`)
// and checks what a Python script sees: several SQLite databases by name, the read-only guard,
// and the native CSV / XLSX export. Skips when no Python interpreter is on PATH.

import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { inflateRawSync } from 'node:zlib';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

const HELPER = resolve(process.cwd(), '../../crates/core/src/python_helper.py');

/** The first Python that prints a real version (skips the Windows Store `python3` alias). */
function findPython(): string | null {
  for (const exe of process.platform === 'win32' ? ['py', 'python', 'python3'] : ['python3', 'python']) {
    const r = spawnSync(exe, ['--version'], { encoding: 'utf8' });
    if (!r.error && `${r.stdout}${r.stderr}`.match(/\d+\.\d+/)) return exe;
  }
  return null;
}

const PYTHON = findPython();

function readZip(buf: Buffer): Map<string, string> {
  const out = new Map<string, string>();
  let i = 0;
  while (i + 4 <= buf.length && buf.readUInt32LE(i) === 0x04034b50) {
    const compSize = buf.readUInt32LE(i + 18);
    const nameLen = buf.readUInt16LE(i + 26);
    const extraLen = buf.readUInt16LE(i + 28);
    const name = buf.toString('utf8', i + 30, i + 30 + nameLen);
    const dataStart = i + 30 + nameLen + extraLen;
    out.set(name, inflateRawSync(buf.subarray(dataStart, dataStart + compSize)).toString('utf8'));
    i = dataStart + compSize;
  }
  return out;
}

let dir = '';

function sqlite(id: string, name: string, file: string) {
  return { id, name, driver: 'sqlite', fields: { path: join(dir, file) }, ssh: false, granted: true };
}

function run(source: string, conns: object[]) {
  const file = join(dir, `s-${Math.random().toString(36).slice(2)}.py`);
  writeFileSync(file, source);
  const r = spawnSync(PYTHON!, [HELPER, file], {
    cwd: dir,
    encoding: 'utf8',
    timeout: 30_000,
    env: { ...process.env, PYTHONUTF8: '1', PYTHONIOENCODING: 'utf-8', BULKDB_CONN_ID: (conns[0] as { id: string }).id, BULKDB_CONN_NAME: (conns[0] as { name: string }).name, BULKDB_DRIVER: 'sqlite', BULKDB_CONNECTIONS: JSON.stringify(conns) },
  });
  return { status: r.status, stderr: r.stderr, lines: r.stdout.trim().split('\n').filter(Boolean) };
}

describe.skipIf(!PYTHON)('python_helper.py', () => {
  beforeAll(() => {
    dir = mkdtempSync(join(tmpdir(), 'bulkdb-py-'));
    const seed = `import sqlite3
a = sqlite3.connect(r'${join(dir, 'kpia1.db')}')
a.executescript("create table users(id integer primary key, name text, status text, left_at text); insert into users values (1,'kim','active',null),(2,'이훈화','active','2026-01-01'),(3,'park','idle',null)")
a.commit(); a.close()
b = sqlite3.connect(r'${join(dir, 'kpia2.db')}')
b.executescript("create table orders(id integer primary key, user_id int, total int); insert into orders values (10,1,500),(11,1,700),(12,3,50),(13,9,1)")
b.commit(); b.close()`;
    const seedFile = join(dir, 'seed.py');
    writeFileSync(seedFile, seed);
    const made = spawnSync(PYTHON!, [seedFile], { encoding: 'utf8' });
    expect(made.status, made.stderr).toBe(0);
  });

  afterAll(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  const twoDbs = () => [sqlite('c1', 'kpia1', 'kpia1.db'), sqlite('c2', 'kpia2', 'kpia2.db'), { id: 'c3', name: 'prod', driver: 'postgres' }];

  it('opens two databases by name and combines them, read-only', () => {
    const out = run(
      `import json
a = bulkdb.db('kpia1')
b = bulkdb.db('kpia2')
users = a.from_('users').select('id', 'name').where({'status': 'active'}).order_by('id', 'desc').fetch()
orders = b.from_('orders').where({'user_id': [u['id'] for u in users]}).order_by('id').fetch()
print(json.dumps({'tab': [bulkdb.name, bulkdb.driver], 'users': users, 'orders': orders,
                  'null': a.from_('users').where({'left_at': None}).count(),
                  'emptyIn': b.from_('orders').where({'user_id': []}).count(),
                  'one': b.from_('orders').order_by('total', 'desc').fetch_one(),
                  'page': b.from_('orders').select('id').order_by('id').limit(2).offset(1).fetch(),
                  'kwargs': a.from_('users').where(status='idle').select('id').fetch(),
                  'conns': bulkdb.connections,
                  'leak': __import__('os').environ.get('BULKDB_CONNECTIONS') is None}, ensure_ascii=False))`,
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    const r = JSON.parse(out.lines[0]);
    expect(r.tab).toEqual(['kpia1', 'sqlite']);
    expect(r.users).toEqual([
      { id: 2, name: '이훈화' },
      { id: 1, name: 'kim' },
    ]);
    expect(r.orders).toEqual([
      { id: 10, user_id: 1, total: 500 },
      { id: 11, user_id: 1, total: 700 },
    ]);
    expect([r.null, r.emptyIn]).toEqual([2, 0]);
    expect(r.one).toEqual({ id: 11, user_id: 1, total: 700 });
    expect(r.page).toEqual([{ id: 11 }, { id: 12 }]);
    expect(r.kwargs).toEqual([{ id: 3 }]);
    expect(r.conns).toEqual([
      { name: 'kpia1', driver: 'sqlite', available: true },
      { name: 'kpia2', driver: 'sqlite', available: true },
      { name: 'prod', driver: 'postgres', available: false },
    ]);
    expect(r.leak).toBe(true); // BULKDB_CONNECTIONS removed from the environment
  });

  it('runs only single read-only statements', () => {
    const blocked = ['DELETE FROM users', "UPDATE users SET name='x'", 'SELECT 1; DROP TABLE users', 'SELECT * INTO copy FROM users'];
    const allowed = [`SELECT name FROM users WHERE name <> 'please update; delete' ORDER BY id`];
    const stmts = [...blocked, ...allowed, 'BUILDER'];
    const out = run(
      `import json
out = []
stmts = ${JSON.stringify(stmts)}
for s in stmts:
    try:
        if s == 'BUILDER':
            bulkdb.from_('users; DROP TABLE users').fetch()
        else:
            bulkdb.query(s)
        out.append('OK')
    except Exception as e:
        out.append('ERR ' + str(e))
print(json.dumps({'r': out, 'count': bulkdb.from_('users').count()}, ensure_ascii=False))`,
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    const r = JSON.parse(out.lines[0]);
    for (let i = 0; i < blocked.length; i++) expect(r.r[i], blocked[i]).toMatch(/^ERR \[kpia1\] 읽기 전용입니다/);
    expect(r.r[blocked.length]).toBe('OK'); // the allowed SELECT ran
    expect(r.r[r.r.length - 1]).toMatch(/^ERR \[kpia1\] 읽기 전용입니다/); // builder injection blocked
    expect(r.count).toBe(3); // nothing was modified
  });

  it('exports arbitrary rows to CSV (BOM) and multi-sheet XLSX', () => {
    const out = run(
      `import json
users = bulkdb.db('kpia1').from_('users').select('id', 'name').fetch()
summary = [{'metric': '합계', 'value': 1250}]
print(json.dumps(bulkdb.to_csv(users, 'u.csv'), ensure_ascii=False))
print(json.dumps(bulkdb.to_xlsx({'사용자': users, '합계': summary}, 'book.xlsx'), ensure_ascii=False))`,
      twoDbs(),
    );
    expect(out.status, out.stderr).toBe(0);
    const csvMeta = JSON.parse(out.lines[0]);
    const bookMeta = JSON.parse(out.lines[1]);
    expect(csvMeta).toEqual({ file: 'u.csv', rows: 3, columns: ['id', 'name'] });
    expect(bookMeta.sheets).toEqual([
      { name: '사용자', rows: 3, columns: ['id', 'name'] },
      { name: '합계', rows: 1, columns: ['metric', 'value'] },
    ]);

    // CSV: BOM + Korean UTF-8.
    const csv = readFileSync(join(dir, 'u.csv'));
    expect(csv.subarray(0, 3)).toEqual(Buffer.from([0xef, 0xbb, 0xbf]));
    expect(csv.toString('utf8')).toContain('이훈화');

    // XLSX: a real ZIP with the expected parts, sheet names and cells.
    const zip = readZip(readFileSync(join(dir, 'book.xlsx')));
    expect([...zip.keys()]).toEqual(['[Content_Types].xml', '_rels/.rels', 'xl/workbook.xml', 'xl/_rels/workbook.xml.rels', 'xl/worksheets/sheet1.xml', 'xl/worksheets/sheet2.xml']);
    expect(zip.get('xl/workbook.xml')).toContain('<sheet name="사용자" sheetId="1" r:id="rId1"/>');
    const sheet1 = zip.get('xl/worksheets/sheet1.xml')!;
    expect(sheet1).toContain('<c r="A1" t="inlineStr"><is><t xml:space="preserve">id</t></is></c>');
    expect(sheet1).toContain('<t xml:space="preserve">이훈화</t>');
    expect(sheet1).toMatch(/<c r="A\d"><v>1<\/v><\/c>/); // id is a number cell
  });
});
