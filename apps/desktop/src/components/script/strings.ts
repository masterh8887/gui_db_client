// Korean / English strings for the Script tab.

const STRINGS = {
  runtimeNode: 'Node.js',
  runtimePython: 'Python',
  runtimeBash: 'Bash',
  notInstalled: '미설치',
  recheck: '다시 확인 (설치했는데도 미설치로 보이면 클릭)',
  install: '설치',
  installing: '설치 중…',
  detecting: '확인 중…',
  run: '▶ 실행',
  stop: '■ 중지',
  clearLog: '지우기',
  stateIdle: '준비',
  stateDone: '완료',
  stateFailed: '실패',
  stateCancelled: '취소됨',
  stateRunning: '실행 중',
  stateInstall: '설치 중',
  exitCode: (code: number) => `exit ${code}`,
  duration: (ms: number) => ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`,
  // `conn` is the tab's connection name, already quoted as a JS string literal.
  placeholderNode: (conn: string) => `// Node.js 스크립트 — bulkdb 가 자동으로 준비됩니다 (읽기 전용: SELECT / WITH 만 실행)
// 드라이버는 한 번만 설치: npm install -g pg | mysql2 | mssql   (SQLite 는 설치 불필요)

// 저장된 연결을 이름으로 엽니다. 한 스크립트에서 여러 DB 를 함께 쓸 수 있습니다.
const a = bulkdb.db(${conn});
const b = bulkdb.db('다른 연결 이름');

const users = await a.from('users').select('id', 'name').where({ status: 'active' }).limit(100).fetch();
const orders = await b.from('orders').where({ user_id: users.map((u) => u.id) }).fetch(); // 배열은 IN (...)

// 엑셀/CSV 로 저장 (오른쪽 패널에서 구조를 만들고 더블클릭으로 삽입할 수 있습니다)
await bulkdb.toXlsx({ 사용자: users, 주문: orders }, 'out.xlsx'); // 시트별로 분리
// await bulkdb.toCsv(users, 'users.csv');

// 직접 SQL:            await a.query('SELECT COUNT(*) AS n FROM users')
// 같은 서버의 다른 DB:  a.use('db이름').from('테이블')
// 이 탭의 연결만 쓸 때: bulkdb.from('테이블') / bulkdb.query('SELECT ...')`,
  placeholderPython: `# Python 스크립트 — bulkdb 가 자동으로 준비됩니다 (읽기 전용: SELECT / WITH 만 실행)
# SQLite 는 설치 불필요. PostgreSQL=psycopg2 · MySQL=pymysql · SQL Server=pyodbc (pip install)

# 저장된 연결을 이름으로 엽니다. 한 스크립트에서 여러 DB 를 함께 쓸 수 있습니다.
a = bulkdb.db('연결 이름')
b = bulkdb.db('다른 연결 이름')

users = a.from_('users').select('id', 'name').where({'status': 'active'}).limit(100).fetch()
orders = b.from_('orders').where({'user_id': [u['id'] for u in users]}).fetch()  # 리스트는 IN (...)

# 엑셀/CSV 로 저장 (오른쪽 패널에서 구조를 만들고 클릭으로 삽입할 수 있습니다)
bulkdb.to_xlsx({'사용자': users, '주문': orders}, 'out.xlsx')  # 시트별로 분리
# bulkdb.to_csv(users, 'users.csv')

# 직접 SQL:            a.query('SELECT COUNT(*) AS n FROM users')
# 같은 서버의 다른 DB:  a.use('db이름').from_('테이블')
# 이 탭의 연결만 쓸 때: bulkdb.from_('테이블') / bulkdb.query('SELECT ...')`,
  placeholderBash: `#!/usr/bin/env bash
# Bash 스크립트 — BULKDB_* 환경변수 사용 가능
# BULKDB_CONNECTIONS: 스크립트에 이름을 적은 다른 연결들의 접속 정보 (JSON 배열)
echo "host: $BULKDB_HOST"`,
  installPrompt: (name: string) => `${name} 런타임이 설치되어 있지 않습니다. 설치하시겠습니까?`,
};

export function ss<K extends keyof typeof STRINGS>(key: K, ...args: (typeof STRINGS)[K] extends (...a: infer A) => unknown ? A : []): string {
  const v = STRINGS[key];
  return typeof v === 'function' ? (v as (...a: unknown[]) => string)(...args) : (v as string);
}

// Strings for the script inspector palette (right 세부 정보 panel).
const INSPECTOR = {
  connections: '연결',
  thisTab: '이 탭',
  connInsertHint: '이 연결을 스크립트에 추가',
  exportTitle: '엑셀 · CSV 내보내기',
  exportFormat: '내보내기 형식',
  exportFile: '파일 이름',
  sheetName: '시트 이름',
  csvOneSheet: 'CSV 는 시트 1개',
  removeSheet: '시트 삭제',
  sheetColumns: '컬럼',
  columnsPlaceholder: '컬럼 (쉼표로 구분): id, name, total',
  addSheet: '시트 추가',
  insertExport: '내보내기 코드 삽입',
  exportSummary: (sheets: string, cols: string) => `시트 ${sheets}개 · 컬럼 ${cols}개 · 데이터 행은 실행 시 채워집니다`,
} as const;

export function is<K extends keyof typeof INSPECTOR>(key: K, ...args: (typeof INSPECTOR)[K] extends (...a: infer A) => unknown ? A : []): string {
  const v = INSPECTOR[key];
  return typeof v === 'function' ? (v as (...a: unknown[]) => string)(...args) : (v as string);
}
