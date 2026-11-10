// Catalog of insertable code for the script inspector palette, one set per runtime. Each snippet
// is dropped into the editor at the cursor; `bulkdb` is the helper the Node runtime preloads.

import type { ScriptRuntime } from 'ipc-types';

export interface Snippet {
  id: string;
  label: string;
  hint: string;
  code: string;
}

export interface SnippetGroup {
  title: string;
  items: Snippet[];
}

/** A saved-connection name turned into a valid JS identifier for `const <id> = bulkdb.db(...)`. */
export function connIdent(name: string): string {
  const base = name
    .normalize('NFKD')
    .replace(/[^A-Za-z0-9_$]+/g, '_')
    .replace(/^_+|_+$/g, '');
  if (!base) return 'db';
  return /^[0-9]/.test(base) ? '_' + base : base;
}

const jsString = (s: string) => `'${s.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`;

const pyString = (s: string) => `'${s.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`;

/** The line that opens a saved connection by name, for the active runtime. */
export function connectionSnippet(runtime: ScriptRuntime, name: string, isTabConn: boolean): string {
  if (runtime === 'node') return `const ${connIdent(name)} = bulkdb.db(${jsString(name)});`;
  if (runtime === 'python') return `${connIdent(name)} = bulkdb.db(${pyString(name)})`;
  return `# ${name}${isTabConn ? ' (이 탭의 연결: $BULKDB_HOST 등)' : ' — BULKDB_CONNECTIONS 에서 접근'}`;
}

// ----- Node: the bulkdb helper ---------------------------------------------------------------

const NODE: SnippetGroup[] = [
  {
    title: '쿼리',
    items: [
      {
        id: 'select',
        label: '쿼리 빌더',
        hint: "bulkdb.from('테이블').select(...).where(...).fetch()",
        code: `const rows = await bulkdb.from('테이블')\n  .select('컬럼1', '컬럼2')\n  .where({ status: 'active' })\n  .limit(100)\n  .fetch();\nconsole.log(rows.length);`,
      },
      { id: 'raw', label: '직접 SQL', hint: "bulkdb.query('SELECT ...', [params])", code: `const rows = await bulkdb.query('SELECT * FROM 테이블 WHERE id = ?', [1]);` },
      { id: 'one', label: '첫 행', hint: '.fetchOne() — 한 행 또는 null', code: `const row = await bulkdb.from('테이블').where({ id: 1 }).fetchOne();` },
      { id: 'count', label: '행 개수', hint: '.count() — 조건에 맞는 행 수', code: `const n = await bulkdb.from('테이블').where({ status: 'active' }).count();\nconsole.log(n);` },
      { id: 'in', label: 'IN 조건', hint: '배열을 넘기면 IN (...)', code: `const rows = await bulkdb.from('테이블').where({ id: [1, 2, 3] }).fetch();` },
      { id: 'otherdb', label: '다른 연결', hint: "bulkdb.db('연결이름') — 다른 저장된 DB", code: `const other = bulkdb.db('연결이름');\nconst rows = await other.from('테이블').fetch();` },
      { id: 'usedb', label: '같은 서버 다른 DB', hint: ".use('db이름')", code: `const db2 = bulkdb.use('db이름');\nconst rows = await db2.from('테이블').fetch();` },
    ],
  },
  {
    title: '엑셀 · CSV 내보내기',
    items: [
      { id: 'xlsx', label: 'XLSX 내보내기', hint: "bulkdb.toXlsx(rows, 'out.xlsx')", code: `await bulkdb.toXlsx(rows, 'out.xlsx');` },
      { id: 'csv', label: 'CSV 내보내기', hint: "bulkdb.toCsv(rows, 'out.csv') — 한글 엑셀 호환", code: `await bulkdb.toCsv(rows, 'out.csv');` },
      {
        id: 'xlsx-multi',
        label: '멀티시트 XLSX',
        hint: '두 DB의 결과를 시트로 분리',
        code: `await bulkdb.toXlsx({\n  '시트1': rows1,\n  '합계': summary,\n}, 'out.xlsx');`,
      },
      {
        id: 'xlsx-cols',
        label: '컬럼 지정 내보내기',
        hint: '출력 컬럼과 순서를 직접 지정',
        code: `await bulkdb.toXlsx(rows, 'out.xlsx', { columns: ['컬럼1', '컬럼2'] });`,
      },
    ],
  },
  {
    title: '패턴 · 유틸',
    items: [
      {
        id: 'two-db',
        label: '2개 DB 결합',
        hint: '한 DB의 결과로 다른 DB를 조회',
        code: `const a = bulkdb.db('연결A');\nconst b = bulkdb.db('연결B');\nconst users = await a.from('users').select('id', 'name').where({ status: 'active' }).fetch();\nconst orders = await b.from('orders').where({ user_id: users.map((u) => u.id) }).fetch();\nconst byUser = new Map(users.map((u) => [u.id, { ...u, total: 0 }]));\nfor (const o of orders) byUser.get(o.user_id).total += o.total;\nconst summary = [...byUser.values()];\nawait bulkdb.toXlsx(summary, 'summary.xlsx');`,
      },
      { id: 'log', label: 'console.log', hint: '값 출력', code: `console.log();` },
      { id: 'json', label: 'JSON 파일로 저장', hint: 'fs.writeFileSync', code: `const fs = require('fs');\nfs.writeFileSync('out.json', JSON.stringify(rows, null, 2));` },
    ],
  },
];

// ----- Python ---------------------------------------------------------------------------------

// Python's bulkdb helper mirrors Node's, but synchronous (no await) and with snake_case names;
// `from_` because `from` is a Python keyword.
const PYTHON: SnippetGroup[] = [
  {
    title: '쿼리',
    items: [
      {
        id: 'select',
        label: '쿼리 빌더',
        hint: "bulkdb.from_('테이블').select(...).where(...).fetch()",
        code: `rows = bulkdb.from_('테이블') \\\n    .select('컬럼1', '컬럼2') \\\n    .where({'status': 'active'}) \\\n    .limit(100) \\\n    .fetch()\nprint(len(rows))`,
      },
      { id: 'raw', label: '직접 SQL', hint: "bulkdb.query('SELECT ...', [params])", code: `rows = bulkdb.query('SELECT * FROM 테이블 WHERE id = ?', [1])  # sqlite/mssql 은 ?, postgres/mysql 은 %s` },
      { id: 'one', label: '첫 행', hint: '.fetch_one() — 한 행 또는 None', code: `row = bulkdb.from_('테이블').where({'id': 1}).fetch_one()` },
      { id: 'count', label: '행 개수', hint: '.count() — 조건에 맞는 행 수', code: `n = bulkdb.from_('테이블').where({'status': 'active'}).count()\nprint(n)` },
      { id: 'in', label: 'IN 조건', hint: '리스트를 넘기면 IN (...)', code: `rows = bulkdb.from_('테이블').where({'id': [1, 2, 3]}).fetch()` },
      { id: 'otherdb', label: '다른 연결', hint: "bulkdb.db('연결이름') — 다른 저장된 DB", code: `other = bulkdb.db('연결이름')\nrows = other.from_('테이블').fetch()` },
      { id: 'usedb', label: '같은 서버 다른 DB', hint: ".use('db이름')", code: `db2 = bulkdb.use('db이름')\nrows = db2.from_('테이블').fetch()` },
    ],
  },
  {
    title: '엑셀 · CSV 내보내기',
    items: [
      { id: 'xlsx', label: 'XLSX 내보내기', hint: "bulkdb.to_xlsx(rows, 'out.xlsx') — 무설치", code: `bulkdb.to_xlsx(rows, 'out.xlsx')` },
      { id: 'csv', label: 'CSV 내보내기', hint: "bulkdb.to_csv(rows, 'out.csv') — 한글 엑셀 호환", code: `bulkdb.to_csv(rows, 'out.csv')` },
      { id: 'xlsx-multi', label: '멀티시트 XLSX', hint: '두 DB의 결과를 시트로 분리', code: `bulkdb.to_xlsx({\n    '시트1': rows1,\n    '합계': summary,\n}, 'out.xlsx')` },
      { id: 'xlsx-cols', label: '컬럼 지정 내보내기', hint: '출력 컬럼과 순서를 직접 지정', code: `bulkdb.to_xlsx(rows, 'out.xlsx', columns=['컬럼1', '컬럼2'])` },
    ],
  },
  {
    title: '패턴 · 유틸',
    items: [
      {
        id: 'two-db',
        label: '2개 DB 결합',
        hint: '한 DB의 결과로 다른 DB를 조회',
        code: `a = bulkdb.db('연결A')\nb = bulkdb.db('연결B')\nusers = a.from_('users').select('id', 'name').where({'status': 'active'}).fetch()\norders = b.from_('orders').where({'user_id': [u['id'] for u in users]}).fetch()\nby_user = {u['id']: dict(u, total=0) for u in users}\nfor o in orders:\n    by_user[o['user_id']]['total'] += o['total']\nsummary = list(by_user.values())\nbulkdb.to_xlsx(summary, 'summary.xlsx')`,
      },
      { id: 'print', label: 'print', hint: '값 출력', code: `print()` },
    ],
  },
];

// ----- Bash -----------------------------------------------------------------------------------

const BASH: SnippetGroup[] = [
  {
    title: '환경변수',
    items: [
      { id: 'env', label: '접속 정보', hint: 'BULKDB_* 변수', code: `echo "host=$BULKDB_HOST port=$BULKDB_PORT db=$BULKDB_DATABASE user=$BULKDB_USER"` },
      { id: 'conns', label: '연결 목록 (jq)', hint: 'BULKDB_CONNECTIONS 는 JSON 배열', code: `echo "$BULKDB_CONNECTIONS" | jq -r '.[].name'` },
    ],
  },
];

export function snippetGroups(runtime: ScriptRuntime): SnippetGroup[] {
  return runtime === 'node' ? NODE : runtime === 'python' ? PYTHON : BASH;
}

// ----- export builder code generation (Node) -------------------------------------------------

export interface ExportSheet {
  name: string;
  /** Column names, in output order. */
  columns: string[];
}

/** The runtimes the export builder can generate code for (Bash has no natural fit). */
export type ExportRuntime = 'node' | 'python';

export function exportCode(sheets: ExportSheet[], filename: string, format: 'xlsx' | 'csv', runtime: ExportRuntime): string {
  return runtime === 'python' ? pythonExport(sheets, filename, format) : nodeExport(sheets, filename, format);
}

function nodeExport(sheets: ExportSheet[], filename: string, format: 'xlsx' | 'csv'): string {
  const file = jsString(filename);
  const colsOpt = (cols: string[]) => (cols.length ? `, { columns: [${cols.map(jsString).join(', ')}] }` : '');
  if (format === 'csv') {
    const cols = sheets[0]?.columns ?? [];
    return `const rows = []; // 여기에 행을 채우세요 (예: await bulkdb.from('테이블').select(...).fetch())\nawait bulkdb.toCsv(rows, ${file}${colsOpt(cols)});`;
  }
  if (sheets.length <= 1) {
    const cols = sheets[0]?.columns ?? [];
    return `const rows = []; // 여기에 행을 채우세요\nawait bulkdb.toXlsx(rows, ${file}${colsOpt(cols)});`;
  }
  const decls = sheets.map((s, i) => `const rows${i + 1} = []; // ${s.name}`).join('\n');
  const body = sheets.map((s, i) => `  ${jsString(s.name)}: rows${i + 1},`).join('\n');
  return `${decls}\nawait bulkdb.toXlsx({\n${body}\n}, ${file});`;
}

function pythonExport(sheets: ExportSheet[], filename: string, format: 'xlsx' | 'csv'): string {
  const file = pyString(filename);
  const colsKw = (cols: string[]) => (cols.length ? `, columns=[${cols.map(pyString).join(', ')}]` : '');
  if (format === 'csv') {
    const cols = sheets[0]?.columns ?? [];
    return `rows = []  # 여기에 행(dict)을 채우세요 (예: bulkdb.from_('테이블').select(...).fetch())\nbulkdb.to_csv(rows, ${file}${colsKw(cols)})`;
  }
  if (sheets.length <= 1) {
    const cols = sheets[0]?.columns ?? [];
    return `rows = []  # 여기에 행(dict)을 채우세요\nbulkdb.to_xlsx(rows, ${file}${colsKw(cols)})`;
  }
  const decls = sheets.map((s, i) => `rows${i + 1} = []  # ${s.name}`).join('\n');
  const body = sheets.map((s, i) => `    ${pyString(s.name)}: rows${i + 1},`).join('\n');
  return `${decls}\nbulkdb.to_xlsx({\n${body}\n}, ${file})`;
}
