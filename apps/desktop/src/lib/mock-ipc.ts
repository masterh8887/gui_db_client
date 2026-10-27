// In-browser fake backend. `ipc.ts` routes every `invoke` / `listen` here when the app is not
// running inside Tauri (plain `next dev`, vitest). It implements every command with realistic
// data and drives jobs through the nine-state machine with timers, emitting the same events the
// Tauri shell emits. Nothing here is imported by production code paths other than `ipc.ts`.

import type {
  AppInfo,
  ColumnInfo,
  Connection,
  ConnectionInput,
  DatabaseInfo,
  DiagnosticsBundle,
  DriverInfo,
  ErrorPayload,
  ExactCount,
  ExportRequest,
  HistoryEntry,
  HostKeyPrompt,
  ImportedConnection,
  IndexInfo,
  JobDone,
  JobProgress,
  JobResult,
  JobServer,
  JobState,
  JobStateEvent,
  JobSummary,
  LogEvent,
  LogLevel,
  PlanNode,
  PlanSummary,
  PlanWarning,
  PreflightIssue,
  PreflightReport,
  ProbeResult,
  SchemaInfo,
  ServerInfo,
  SessionInfo,
  Settings,
  SqlCheck,
  SqlFile,
  SshPasswordPrompt,
  StatementKind,
  TableInfo,
  TableRef,
} from 'ipc-types';
import { joinPath, num, parseMemSize } from './format';
import { IpcError } from './ipc-error';
import {
  LOCAL_CONN_ID,
  PROD_CONN_ID,
  mockColumns,
  mockConnections,
  mockDatabases,
  mockDdl,
  mockDrivers,
  mockFindTable,
  mockHistory,
  mockIndexes,
  mockSchemas,
  mockSelectSql,
  mockSettings,
  mockTables,
} from './mock-data';

// Event names are repeated here (instead of importing `./events`) to keep the module graph
// acyclic: `events.ts` -> `ipc.ts` -> `mock-ipc.ts`.
const EV = {
  jobState: 'job:state',
  jobProgress: 'job:progress',
  jobServer: 'job:server',
  jobDone: 'job:done',
  jobLog: 'job:log',
  tunnelHostKey: 'tunnel:hostkey',
  tunnelPassword: 'tunnel:password',
  driversChanged: 'drivers:changed',
} as const;

export interface MockOptions {
  /** Simulated round-trip latency per call. */
  latencyMs: number;
  /** `job:progress` cadence. */
  progressTickMs: number;
  /** Time spent in phase `preparing` once running. */
  prepareMs: number;
  /** `job:server` cadence. */
  serverIntervalMs: number;
  /** Time spent in `finishing`. */
  finishingMs: number;
  /** Duration of an exact-count job. */
  exactCountMs: number;
  /** Rows an export produces (capped by the table estimate when it is smaller). */
  totalRows: number;
  /** Transfer duration (after `prepareMs`) for `totalRows`. */
  runMs: number;
  /** Output bytes per row. */
  rowWidth: number;
}

const DEFAULT_OPTIONS: MockOptions = {
  latencyMs: 40,
  progressTickMs: 200,
  prepareMs: 2000,
  serverIntervalMs: 3000,
  finishingMs: 600,
  exactCountMs: 3000,
  totalRows: 2_000_000,
  runMs: 10_000,
  rowWidth: 118,
};

const SERVER_RAM_BYTES = 8 * 1024 ** 3;
const DISK_FREE_BYTES = 250 * 1024 ** 3;
const WORK_MEM_HIGH_BYTES = 256 * 1024 ** 2;
const TERMINAL: JobState[] = ['done', 'cancelled', 'failed', 'rejected'];

// ---- event bus ---------------------------------------------------------------------------

type Listener = (payload: unknown) => void;
const listeners = new Map<string, Set<Listener>>();

export function mockListen<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
  let set = listeners.get(event);
  if (!set) {
    set = new Set();
    listeners.set(event, set);
  }
  const l = handler as Listener;
  set.add(l);
  return Promise.resolve(() => {
    set.delete(l);
  });
}

function emit(event: string, payload: unknown): void {
  const set = listeners.get(event);
  if (!set || set.size === 0) return;
  const snapshot = [...set];
  queueMicrotask(() => {
    for (const l of snapshot) {
      try {
        l(payload);
      } catch (e) {
        console.error(`[mock-ipc] listener for ${event} threw`, e);
      }
    }
  });
}

// ---- timers (all tracked so `reset()` can clear them) --------------------------------------

type Handle = ReturnType<typeof setTimeout>;
const timers = new Set<Handle>();

function after(ms: number, fn: () => void): Handle {
  const h = setTimeout(() => {
    timers.delete(h);
    fn();
  }, ms);
  timers.add(h);
  return h;
}

function every(ms: number, fn: () => void): Handle {
  const h = setInterval(fn, ms);
  timers.add(h);
  return h;
}

function clearHandle(h: Handle | undefined): void {
  if (h === undefined) return;
  clearTimeout(h);
  clearInterval(h);
  timers.delete(h);
}

function sleep(ms: number): Promise<void> {
  if (ms <= 0) return Promise.resolve();
  return new Promise((r) => after(ms, r));
}

// ---- state -------------------------------------------------------------------------------

interface MockJob {
  summary: JobSummary;
  req?: ExportRequest;
  acks: Set<string>;
  startedMs: number;
  runStartMs?: number;
  rows: number;
  totalRows: number;
  outputPath: string;
  progressTimer?: Handle;
  serverTimer?: Handle;
  stepTimer?: Handle;
  serverTicks: number;
  tick: number;
}

interface PendingPrompt {
  kind: 'hostkey' | 'secret';
  resolve: (v: boolean | string | undefined) => void;
}

interface MockState {
  connections: Connection[];
  /** Passwords "in the keyring" (connections saved with `savePassword`). */
  passwords: Map<string, string>;
  /** Passwords typed this session for "ask every time" connections (core `session_secrets`). */
  sessionSecrets: Map<string, string>;
  sshSecrets: Map<string, string>;
  drivers: DriverInfo[];
  /**
   * Vendor clients "installed" since the last probe (`mockBackend.installDriver`). Like the
   * core registry, the driver list only picks them up on `drivers_rescan`.
   */
  installedDrivers: Set<string>;
  settings: Settings;
  history: HistoryEntry[];
  jobs: Map<string, MockJob>;
  logs: LogEvent[];
  writtenFiles: Set<string>;
  knownHosts: Set<string>;
  prompts: Map<string, PendingPrompt>;
  /**
   * The last finished exact count per connection and table (core keeps these in the history
   * database, `catalog_last_exact_count` reads them back). Keyed by `exactCountKey`.
   */
  exactCounts: Map<string, ExactCount>;
  seq: number;
}

let options: MockOptions = { ...DEFAULT_OPTIONS };
let state: MockState = freshState();
const calls: string[] = [];

function freshState(): MockState {
  return {
    connections: mockConnections(),
    passwords: new Map([['c-prod', 'stored-in-keyring']]),
    sessionSecrets: new Map(),
    sshSecrets: new Map(),
    drivers: mockDrivers(),
    installedDrivers: new Set(),
    settings: mockSettings(),
    history: mockHistory(),
    jobs: new Map(),
    logs: [],
    writtenFiles: new Set(mockHistory().flatMap((h) => h.files)),
    knownHosts: new Set(),
    prompts: new Map(),
    exactCounts: new Map(),
    seq: 0,
  };
}

function nowIso(): string {
  return new Date().toISOString();
}

function nextId(prefix: string): string {
  state.seq += 1;
  return `${prefix}-${state.seq.toString().padStart(4, '0')}`;
}

function err(code: string, message: string, detail?: string): IpcError {
  return new IpcError({ code, message, detail });
}

function log(level: LogLevel, message: string, jobId?: string, target = 'bulkdb::mock'): void {
  const ev: LogEvent = { jobId, level, message, at: nowIso(), target };
  state.logs.push(ev);
  if (state.logs.length > 2000) state.logs.splice(0, state.logs.length - 2000);
  emit(EV.jobLog, ev);
}

function findConn(connId: string): Connection {
  const c = state.connections.find((x) => x.id === connId);
  if (!c) throw err('NOT_FOUND', `Connection ${connId} not found`);
  return c;
}

/**
 * Which canned catalog a connection browses: the two seeded connections have their own; any
 * connection created in the session borrows the SQLite one or the server one by driver.
 */
function catalogIdFor(connId: string): string {
  if (connId === PROD_CONN_ID || connId === LOCAL_CONN_ID) return connId;
  const driver = state.connections.find((c) => c.id === connId)?.driver;
  return driver === 'sqlite' ? LOCAL_CONN_ID : PROD_CONN_ID;
}

/** Whether the connection's driver authenticates with a password (registry `fields`). */
function usesPassword(conn: Connection): boolean {
  return !!state.drivers.find((d) => d.id === conn.driver)?.fields.includes('password');
}

/**
 * Mirror of core `open_connection`: a password-based connection needs either a stored password,
 * one typed earlier this session, or one passed with the call.
 */
function requireAuth(conn: Connection, password?: string): void {
  if (!usesPassword(conn)) return;
  if (password || state.passwords.has(conn.id) || state.sessionSecrets.has(conn.id)) return;
  throw err('AUTH', `Password required for ${conn.name}: it is not stored ("ask every time")`);
}

/** Remember a password typed for an "ask every time" connection (core `remember_secret`). */
function rememberSecret(conn: Connection, password?: string): void {
  if (password && !conn.savePassword) state.sessionSecrets.set(conn.id, password);
}

function connLabel(c: Connection): string {
  return c.name.replace(/[^\w-]+/g, '_').replace(/^_+|_+$/g, '').slice(0, 24) || 'conn';
}

function mockSqlFileContent(path: string): string {
  return `-- ${path}\nSELECT *\nFROM public.orders\nWHERE placed_at >= '2026-10-01'\n`;
}

// ---- SQL guard (port of the core rules that matter for the UI) ------------------------------

/**
 * Lexing rules of the server that runs a query, as core `sqlguard::Dialect::from_driver`:
 * MySQL / MariaDB read `#` comments and backslash escapes, unknown back ends (ODBC) must pass
 * every reading.
 */
export type MockDialect = 'standard' | 'mysql' | 'any';

export function mockDialect(driver: string): MockDialect {
  switch (driver.trim().toLowerCase()) {
    case 'mysql':
    case 'mariadb':
      return 'mysql';
    case 'postgres':
    case 'postgresql':
    case 'mssql':
    case 'sqlite':
    case 'oracle':
      return 'standard';
    default:
      return 'any';
  }
}

interface Scanned {
  /** Statements with comments removed, trimmed, empty ones dropped. */
  statements: string[];
  /** A MySQL executable comment (`/*! ... *\/`, `/*M! ... *\/`) was found. */
  execComment: boolean;
}

/** Split on `;` outside quotes and comments, reading quotes and comments like `mysql` says. */
function scanStatements(sql: string, mysql: boolean): Scanned {
  const statements: string[] = [];
  let execComment = false;
  let cur = '';
  let quote: string | null = null;
  let i = 0;
  const push = () => {
    if (cur.trim()) statements.push(cur.trim());
    cur = '';
  };
  while (i < sql.length) {
    const ch = sql[i];
    const next = sql[i + 1];
    if (quote) {
      cur += ch;
      if (mysql && ch === '\\' && quote !== '`' && i + 1 < sql.length) {
        cur += next;
        i += 2;
        continue;
      }
      if (ch === quote) quote = null;
      i += 1;
      continue;
    }
    if (ch === "'" || ch === '"' || ch === '`') {
      quote = ch;
      cur += ch;
      i += 1;
      continue;
    }
    // MySQL needs whitespace after `--`; `#` starts a comment there too.
    const dashes = ch === '-' && next === '-' && (!mysql || i + 2 >= sql.length || /\s/.test(sql[i + 2]));
    if (dashes || (mysql && ch === '#')) {
      while (i < sql.length && sql[i] !== '\n') i += 1;
      continue;
    }
    if (ch === '/' && next === '*') {
      if (mysql && (sql[i + 2] === '!' || (sql[i + 2] === 'M' && sql[i + 3] === '!'))) execComment = true;
      const end = sql.indexOf('*/', i + 2);
      i = end < 0 ? sql.length : end + 2;
      cur += ' ';
      continue;
    }
    if (ch === ';') {
      push();
      i += 1;
      continue;
    }
    cur += ch;
    i += 1;
  }
  push();
  return { statements, execComment };
}

function issue(code: string, message: string, severity: PreflightIssue['severity'] = 'danger', recommendation?: string): PreflightIssue {
  return { code, severity, message, recommendation };
}

const STATEMENT_KINDS: Record<string, StatementKind> = { select: 'select', with: 'with', values: 'values', table: 'table', explain: 'explain' };

/** One reading of the query (standard or MySQL rules). */
function checkWith(sql: string, mysql: boolean): SqlCheck {
  const { statements: stmts, execComment } = scanStatements(sql, mysql);
  if (execComment) {
    const word = (/^\s*([a-z_]+)/i.exec(stmts[0] ?? '')?.[1] ?? '').toLowerCase();
    return {
      ok: false,
      kind: stmts.length === 0 ? 'empty' : (STATEMENT_KINDS[word] ?? 'other'),
      statements: stmts.length,
      issues: [issue('SQL_EXEC_COMMENT', 'The query contains a MySQL executable comment (/*! ... */); the server runs its content as SQL', 'danger', 'Remove the executable comment, or write its content as plain SQL.')],
      normalized: stmts[0] ?? sql.trim(),
    };
  }
  if (stmts.length === 0) {
    return { ok: false, kind: 'empty', statements: 0, issues: [issue('SQL_EMPTY', 'The query is empty')], normalized: '' };
  }
  const issues: PreflightIssue[] = [];
  if (stmts.length > 1) {
    issues.push(issue('MULTI_STATEMENT', `${stmts.length} statements found; exactly one SELECT is allowed`, 'danger', 'Keep a single SELECT or CTE; remove the others.'));
  }
  const first = stmts[0];
  const word = (/^\s*([a-z_]+)/i.exec(first)?.[1] ?? '').toLowerCase();
  const kind: StatementKind = STATEMENT_KINDS[word] ?? 'other';
  if (kind === 'other' || kind === 'explain') {
    issues.push(issue('SQL_NOT_SELECT', `Only SELECT / WITH / VALUES / TABLE statements can be extracted (found ${word.toUpperCase() || 'unknown'})`, 'danger'));
  } else if (kind === 'with' && /\b(insert|update|delete|merge)\b/i.test(first)) {
    issues.push(issue('SQL_NOT_SELECT', 'A CTE that modifies data cannot be extracted', 'danger'));
  }
  return { ok: issues.length === 0, kind, statements: stmts.length, issues, normalized: first.replace(/;\s*$/, '') };
}

/**
 * Core `sqlguard::check_for`: the query must be one read-only statement under the dialect's
 * rules (for `any`, under the standard and the MySQL reading alike).
 */
export function mockSqlCheck(sql: string, dialect: MockDialect = 'standard'): SqlCheck {
  const first = checkWith(sql, dialect === 'mysql');
  if (dialect !== 'any' || !first.ok) return first;
  const alt = checkWith(sql, true);
  return alt.ok || alt.statements === 0 ? first : { ...alt, normalized: first.normalized };
}

// ---- plan / server -------------------------------------------------------------------------

function referencedTable(connId: string, sql: string): { ref: TableRef; info?: TableInfo } {
  const m = /\bfrom\s+(?:"?([a-z_][\w]*)"?\.)?"?([a-z_][\w]*)"?/i.exec(sql);
  const ref: TableRef = m ? { schema: m[1] || undefined, name: m[2] } : { name: 'query' };
  const info = m ? mockFindTable(catalogIdFor(connId), ref) : undefined;
  if (info && !ref.schema) ref.schema = info.schema;
  return { ref, info };
}

function serverInfo(conn: Connection): ServerInfo {
  if (conn.driver === 'sqlite') {
    return {
      driver: 'sqlite',
      version: '3.46.0',
      database: conn.fields.path,
      settings: { page_size: '4096', journal_mode: 'wal', cache_size: '-2000' },
      canWrite: true,
      notes: ['Local file: no server resources to protect'],
    };
  }
  return {
    driver: conn.driver,
    version: '16.4',
    user: conn.user,
    database: conn.database,
    ramBytes: BigInt(SERVER_RAM_BYTES),
    ramSource: 'shared_buffers x 4',
    settings: {
      work_mem: '4MB',
      shared_buffers: '2GB',
      statement_timeout: '0',
      max_parallel_workers_per_gather: '2',
      server_version: '16.4',
    },
    canWrite: false,
    notes: ['Read-only role: recommended for extraction'],
  };
}

function buildPlan(connId: string, sql: string, analyze: boolean): PlanSummary {
  const { ref, info } = referencedTable(connId, sql);
  const rows = num(info?.estimatedRows) ?? options.totalRows;
  const width = info ? Math.round(num(info.tableBytes)! / Math.max(1, rows)) : options.rowWidth;
  const hasWhere = /\bwhere\b/i.test(sql);
  const outRows = hasWhere ? Math.max(1, Math.round(rows * 0.08)) : rows;
  const scan: PlanNode = {
    nodeType: 'Seq Scan',
    relation: ref,
    alias: ref.name,
    planRows: outRows,
    planWidth: width,
    startupCost: 0,
    totalCost: Math.round(rows * 0.0125 * 100) / 100,
    actualRows: analyze ? outRows : undefined,
    actualLoops: analyze ? 1 : undefined,
    actualTimeMs: analyze ? Math.round(rows / 4000) : undefined,
    children: [],
    extra: hasWhere ? { Filter: sql.slice(sql.search(/\bwhere\b/i) + 6).trim().slice(0, 80) } : {},
  };
  const warnings: PlanWarning[] = [];
  let root = scan;
  if (/\blateral\b/i.test(sql)) {
    root = {
      nodeType: 'Nested Loop',
      planRows: outRows,
      planWidth: width + 40,
      startupCost: 0,
      totalCost: Math.round(rows * 0.9 * 100) / 100,
      children: [
        scan,
        {
          nodeType: 'Limit',
          parentRelationship: 'Inner',
          planRows: 1,
          planWidth: 40,
          totalCost: 8.3,
          children: [{ nodeType: 'Index Scan', relation: { schema: 'public', name: 'order_items' }, planRows: 4, planWidth: 40, totalCost: 8.3, children: [], extra: { 'Index Cond': '(order_id = orders.id)' } }],
          extra: {},
        },
      ],
      extra: {},
    };
    warnings.push({
      code: 'LATERAL_PATTERN',
      severity: 'danger',
      message: `LATERAL subquery runs once per outer row (${outRows.toLocaleString('en-US')} rows): this is the pattern behind the 2026-10-23 OOM`,
      detail: 'Rewrite as a JOIN with an aggregate, or add a LIMIT to the outer query.',
    });
  }
  if (!hasWhere && rows >= 10_000_000) {
    warnings.push({ code: 'SEQ_SCAN_LARGE', severity: 'warning', message: `Sequential scan of ${rows.toLocaleString('en-US')} rows (no WHERE clause)` });
  }
  if (/\border\s+by\b/i.test(sql) && outRows > 1_000_000) {
    warnings.push({ code: 'SORT_SPILL', severity: 'warning', message: `Sorting ${outRows.toLocaleString('en-US')} rows will spill to disk with work_mem=4MB`, detail: 'Raise work_mem for this session or drop ORDER BY (file order rarely matters).' });
    root = { nodeType: 'Sort', planRows: outRows, planWidth: width, totalCost: root.totalCost! * 1.6, children: [root], extra: { 'Sort Key': 'placed_at' } };
  }
  const raw = JSON.stringify([{ Plan: root, 'Planning Time': 0.412, ...(analyze ? { 'Execution Time': Math.round(rows / 4000) } : {}) }], null, 2);
  return {
    estimatedRows: BigInt(outRows),
    estimatedWidth: width,
    totalCost: root.totalCost,
    estimatedBytes: BigInt(outRows * width),
    analyzed: analyze,
    elapsedMs: BigInt(analyze ? 1800 : 120),
    warnings,
    tables: info ? [ref] : [],
    root,
    raw,
    rawFormat: 'json',
  };
}

/** Tables an ETL session holds open in a long transaction (triggers `CONCURRENT_SESSION`). */
const ETL_LOCKED_TABLES = new Set(['order_items', 'events', 'funnel_events', 'page_views']);

function sessionsFor(conn: Connection, ref: TableRef): SessionInfo[] {
  if (conn.driver === 'sqlite') return [];
  const table = `${ref.schema ?? 'public'}.${ref.name}`;
  const sessions: SessionInfo[] = [
    { pid: '48102', user: 'app', application: 'api', state: 'active', query: `SELECT * FROM ${table} WHERE id = $1`, elapsedMs: BigInt(12), tables: [table], blocking: false },
    { pid: '47990', user: 'readonly', application: 'bulkdb-mock', state: 'idle', elapsedMs: BigInt(2_400_000), tables: [], blocking: false },
  ];
  if (ETL_LOCKED_TABLES.has(ref.name)) {
    sessions.splice(1, 0, {
      pid: '48377',
      user: 'etl',
      application: 'airflow',
      state: 'idle in transaction',
      waitEvent: 'ClientRead',
      query: `UPDATE ${table} SET status = $1 WHERE id = ANY($2)`,
      elapsedMs: BigInt(182_000),
      tables: [table],
      blocking: false,
    });
  }
  return sessions;
}

function resolveOutputPath(conn: Connection, req: ExportRequest, label: string): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, '0');
  const yyyyMMdd = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}`;
  const HHmm = `${p(d.getHours())}${p(d.getMinutes())}`;
  const HHmmss = `${HHmm}${p(d.getSeconds())}`;
  let name = req.output.namePattern || '{conn}_{name}_{yyyyMMdd_HHmm}';
  name = name
    .replaceAll('{conn}', connLabel(conn))
    .replaceAll('{name}', label.replace(/[^\w-]+/g, '_'))
    .replaceAll('{yyyyMMdd_HHmm}', `${yyyyMMdd}_${HHmm}`)
    .replaceAll('{yyyyMMdd}', yyyyMMdd)
    .replaceAll('{HHmmss}', HHmmss);
  if (req.output.split && (req.output.split.rows || req.output.split.megabytes)) name += '_part0001';
  name += req.output.format === 'parquet' ? '.parquet' : '.csv';
  if (req.output.format === 'csv' && req.output.compress) name += req.output.compress === 'gzip' ? '.gz' : '.zst';
  const dir = req.output.dir || conn.outputDir || state.settings.defaultOutputDir || 'C:\\exports';
  return joinPath(dir, name);
}

function deriveLabel(req: ExportRequest, connId: string): string {
  if (req.name?.trim()) return req.name.trim();
  if (req.sqlFile) return req.sqlFile.replace(/^.*[\\/]/, '').replace(/\.sql$/i, '');
  const { ref } = referencedTable(connId, req.sql ?? '');
  return ref.name === 'query' ? 'query' : ref.name;
}

function buildPreflight(conn: Connection, req: ExportRequest): PreflightReport {
  const sql = req.sqlFile ? mockSqlFileContent(req.sqlFile) : (req.sql ?? '');
  const check = mockSqlCheck(sql, mockDialect(conn.driver));
  const blockers: PreflightIssue[] = [...check.issues];
  const warnings: PreflightIssue[] = [];
  const label = deriveLabel(req, conn.id);
  const outputPath = resolveOutputPath(conn, req, label);
  const { ref, info } = referencedTable(conn.id, sql);
  const plan = check.ok ? buildPlan(conn.id, sql, false) : undefined;
  const rows = num(plan?.estimatedRows) ?? options.totalRows;
  const width = plan?.estimatedWidth ?? options.rowWidth;
  const bytes = rows * width;

  const wm = req.session.workMem;
  const wmBytes = parseMemSize(wm);
  if (conn.driver !== 'sqlite' && wmBytes !== undefined && wmBytes >= WORK_MEM_HIGH_BYTES) {
    const pct = Math.round((wmBytes / SERVER_RAM_BYTES) * 1000) / 10;
    warnings.push(issue('WORK_MEM_HIGH', `work_mem=${wm} is ${pct}% of the server RAM (8 GB); a sort or hash in this query may push the server into swap`, 'warning', 'Use 128MB (RAM / 64) or less for this session.'));
  }
  if (req.output.overwrite || state.writtenFiles.has(outputPath)) {
    warnings.push(issue('OVERWRITE', `${outputPath} already exists and will be overwritten`, 'warning', 'Change the file name pattern to keep the existing file.'));
  }
  for (const w of plan?.warnings ?? []) {
    if (w.code === 'LATERAL_PATTERN') warnings.push(issue('PLAN_LATERAL', w.message, 'danger', w.detail));
  }
  const sessions = check.ok ? sessionsFor(conn, ref) : [];
  const stuck = sessions.filter((s) => s.blocking || s.state === 'idle in transaction');
  if (stuck.length > 0) {
    warnings.push(issue('CONCURRENT_SESSION', `${stuck.length} session(s) hold ${ref.schema ?? ''}.${ref.name} in an open transaction (pid ${stuck.map((s) => s.pid).join(', ')})`, 'warning', 'A long idle-in-transaction session can hold locks; check before extracting.'));
  }
  if (bytes > DISK_FREE_BYTES) {
    blockers.push(issue('DISK_LOW', `Estimated output ${(bytes / 1024 ** 3).toFixed(1)} GB exceeds the free space ${(DISK_FREE_BYTES / 1024 ** 3).toFixed(0)} GB on the output drive`, 'danger', 'Add a WHERE clause, enable compression or split to another drive.'));
  }
  if (!state.drivers.find((d) => d.id === conn.driver)?.available) {
    blockers.push(issue('DRIVER_MISSING', `Driver ${conn.driver} is not available on this machine`, 'danger'));
  }
  const splitRows = num(req.output.split?.rows);
  const files = splitRows && splitRows > 0 ? Math.max(1, Math.ceil(rows / splitRows)) : 1;
  return {
    warnings,
    blockers,
    estimates: {
      rows: BigInt(rows),
      bytes: BigInt(bytes),
      diskFreeBytes: BigInt(DISK_FREE_BYTES),
      outputPath,
      files,
      workMem: conn.driver === 'sqlite' ? undefined : (wm ?? '4MB'),
      recommendedWorkMem: conn.driver === 'sqlite' ? undefined : '128MB',
      serverRamBytes: conn.driver === 'sqlite' ? undefined : BigInt(SERVER_RAM_BYTES),
    },
    plan,
    server: serverInfo(conn),
    sessions,
    elapsedMs: BigInt(230 + (info ? 40 : 0)),
  };
}

// ---- job engine --------------------------------------------------------------------------

function setState(job: MockJob, s: JobState): void {
  job.summary.state = s;
  if (TERMINAL.includes(s)) job.summary.finishedAt = nowIso();
  const ev: JobStateEvent = { jobId: job.summary.id, state: s, at: nowIso() };
  emit(EV.jobState, ev);
}

function clearJobTimers(job: MockJob): void {
  clearHandle(job.progressTimer);
  clearHandle(job.serverTimer);
  clearHandle(job.stepTimer);
  job.progressTimer = job.serverTimer = job.stepTimer = undefined;
}

function pushHistory(job: MockJob): void {
  const s = job.summary;
  if (!job.req) return;
  const entry: HistoryEntry = {
    id: BigInt(1000 + state.seq),
    jobId: s.id,
    connId: s.connId,
    startedAt: s.startedAt,
    finishedAt: s.finishedAt,
    state: s.state,
    label: s.label,
    sql: job.req.sql ?? '',
    sqlFile: job.req.sqlFile,
    session: job.req.session,
    output: job.req.output,
    rows: s.result?.rows,
    bytes: s.result?.bytes,
    durationMs: s.result?.durationMs ?? BigInt(Date.now() - job.startedMs),
    files: s.result?.files ?? [],
    error: s.error ? `${s.error.code}: ${s.error.message}` : undefined,
  };
  state.history.unshift(entry);
}

function finish(job: MockJob, s: JobState, out: { result?: JobResult; error?: ErrorPayload; exactCount?: ExactCount }): void {
  clearJobTimers(job);
  job.summary.result = out.result;
  job.summary.error = out.error;
  job.summary.exactCount = out.exactCount;
  job.summary.preflight = undefined;
  setState(job, s);
  const done: JobDone = { jobId: job.summary.id, state: s, result: out.result, error: out.error, exactCount: out.exactCount };
  emit(EV.jobDone, done);
  if (job.req) pushHistory(job);
}

function progressSnapshot(job: MockJob, phase: JobProgress['phase']): JobProgress {
  const now = Date.now();
  const elapsedMs = now - job.startedMs;
  const transferMs = job.runStartMs ? Math.max(0, now - job.runStartMs - options.prepareMs) : 0;
  const rate = transferMs > 0 ? job.rows / (transferMs / 1000) : 0;
  const bytesOut = job.rows * options.rowWidth;
  const remaining = job.totalRows - job.rows;
  return {
    jobId: job.summary.id,
    rows: BigInt(job.rows),
    bytesIn: BigInt(Math.round(bytesOut * 1.02)),
    bytesOut: BigInt(bytesOut),
    batches: BigInt(Math.ceil(job.rows / 65536)),
    files: job.rows > 0 ? 1 : 0,
    elapsedMs: BigInt(elapsedMs),
    rateRowsPerSec: Math.round(rate),
    rateBytesPerSec: Math.round(rate * options.rowWidth),
    etaSec: rate > 0 && remaining > 0 ? BigInt(Math.ceil(remaining / rate)) : undefined,
    phase,
  };
}

function progressTick(job: MockJob): void {
  if (job.summary.state !== 'running') return;
  job.tick += 1;
  const runElapsed = Date.now() - (job.runStartMs ?? Date.now());
  let phase: JobProgress['phase'] = 'preparing';
  if (runElapsed >= options.prepareMs) {
    const frac = Math.min(1, (runElapsed - options.prepareMs) / options.runMs);
    job.rows = Math.min(job.totalRows, Math.floor(job.totalRows * frac));
    phase = job.tick % 15 >= 10 && job.tick % 15 <= 11 && job.rows < job.totalRows ? 'network_bound' : 'transferring';
  }
  const p = progressSnapshot(job, phase);
  job.summary.progress = p;
  emit(EV.jobProgress, p);
  if (job.rows >= job.totalRows) {
    clearHandle(job.progressTimer);
    clearHandle(job.serverTimer);
    job.progressTimer = job.serverTimer = undefined;
    setState(job, 'finishing');
    const fin = progressSnapshot(job, 'finishing');
    job.summary.progress = fin;
    emit(EV.jobProgress, fin);
    log('info', `Flushing ${job.outputPath}`, job.summary.id, 'bulkdb::sink');
    job.stepTimer = after(options.finishingMs, () => {
      const durationMs = Date.now() - job.startedMs;
      const result: JobResult = {
        rows: BigInt(job.rows),
        bytes: BigInt(job.rows * options.rowWidth),
        files: [job.outputPath],
        durationMs: BigInt(durationMs),
        verifiedRows: state.settings.verifyAfterExport ? BigInt(job.rows) : undefined,
        notes: [job.req?.output.format === 'parquet' ? 'Parquet row groups: 128 MB' : 'CSV header written', 'File sizes verified on disk'],
      };
      state.writtenFiles.add(job.outputPath);
      log('info', `Done: ${job.rows.toLocaleString('en-US')} rows -> ${job.outputPath}`, job.summary.id, 'bulkdb::job');
      finish(job, 'done', { result });
    });
  }
}

function serverTick(job: MockJob): void {
  if (job.summary.state !== 'running') return;
  job.serverTicks += 1;
  const waits: (string | undefined)[] = [undefined, 'ClientWrite', undefined, 'DataFileRead'];
  const wait = waits[job.serverTicks % waits.length];
  const sv: JobServer = {
    jobId: job.summary.id,
    pid: '48213',
    state: 'active',
    waitEvent: wait,
    serverRows: BigInt(Math.min(job.totalRows, job.rows + 65_536)),
    tempBytes: BigInt(job.rows > 0 ? 32 * 1024 ** 2 : 0),
    bottleneck: wait === 'ClientWrite' ? 'network' : 'server',
    elapsedMs: BigInt(Date.now() - job.startedMs),
  };
  job.summary.server = sv;
  emit(EV.jobServer, sv);
}

function beginRunning(job: MockJob): void {
  job.summary.preflight = undefined;
  job.runStartMs = Date.now();
  setState(job, 'running');
  const sess = job.req?.session;
  log('info', `Session configured: work_mem=${sess?.workMem ?? 'default'} statement_timeout=${num(sess?.statementTimeoutMs) ?? 0}ms`, job.summary.id, 'bulkdb::source');
  log('info', `COPY (${(job.req?.sql ?? '').replace(/\s+/g, ' ').slice(0, 60)}...) TO STDOUT started`, job.summary.id, 'bulkdb::source');
  job.summary.progress = progressSnapshot(job, 'preparing');
  job.progressTimer = every(options.progressTickMs, () => progressTick(job));
  // A local SQLite file has no server session to watch.
  if (state.connections.find((c) => c.id === job.summary.connId)?.driver !== 'sqlite') {
    job.serverTimer = every(options.serverIntervalMs, () => serverTick(job));
  }
}

function evaluatePreflight(job: MockJob, conn: Connection): void {
  const req = job.req!;
  const report = buildPreflight(conn, req);
  job.totalRows = Math.min(options.totalRows, num(report.estimates.rows) ?? options.totalRows);
  job.outputPath = report.estimates.outputPath;
  if (report.blockers.length > 0) {
    const first = report.blockers[0];
    const error: ErrorPayload = { code: first.code === 'DRIVER_MISSING' ? 'DRIVER_MISSING' : first.code === 'DISK_LOW' ? 'IO' : 'INVALID', message: first.message, detail: report.blockers.map((b) => b.code).join(', ') };
    log('error', `Rejected: ${first.message}`, job.summary.id, 'bulkdb::preflight');
    finish(job, 'rejected', { error });
    return;
  }
  const unacked = report.warnings.filter((w) => !job.acks.has(w.code));
  if (unacked.length > 0) {
    job.summary.preflight = report;
    log('warn', `Waiting for acknowledgement: ${unacked.map((w) => w.code).join(', ')}`, job.summary.id, 'bulkdb::preflight');
    setState(job, 'warned');
    return;
  }
  beginRunning(job);
}

function startExportJob(req: ExportRequest, acks: string[]): string {
  const conn = findConn(req.connId);
  if (!req.sql?.trim() && !req.sqlFile) throw err('INVALID', 'Either sql or sqlFile must be set');
  const active = [...state.jobs.values()].filter((j) => !TERMINAL.includes(j.summary.state) && j.summary.kind === 'export');
  if (active.length >= state.settings.maxConcurrentJobs) {
    throw err('INVALID', `Already running ${active.length} job(s); max_concurrent_jobs=${state.settings.maxConcurrentJobs}`);
  }
  const id = nextId('job');
  const label = deriveLabel(req, conn.id);
  const job: MockJob = {
    summary: { id, kind: 'export', state: 'pending', connId: conn.id, connName: conn.name, label, startedAt: nowIso(), request: req },
    req,
    acks: new Set(acks),
    startedMs: Date.now(),
    rows: 0,
    totalRows: options.totalRows,
    outputPath: '',
    serverTicks: 0,
    tick: 0,
  };
  state.jobs.set(id, job);
  log('info', `Job ${id} queued on ${conn.name} (${label})`, id, 'bulkdb::job');
  setState(job, 'pending');
  job.stepTimer = after(150, () => {
    setState(job, 'preflight');
    log('info', 'Preflight: checking SQL, plan, work_mem, sessions, disk', id, 'bulkdb::preflight');
    job.stepTimer = after(300, () => evaluatePreflight(job, conn));
  });
  return id;
}

function cancelJob(id: string): void {
  const job = state.jobs.get(id);
  if (!job) throw err('NOT_FOUND', `Job ${id} not found`);
  if (TERMINAL.includes(job.summary.state)) throw err('INVALID', `Job ${id} is already ${job.summary.state}`);
  const started = job.runStartMs !== undefined;
  clearJobTimers(job);
  const result: JobResult | undefined = started && job.req
    ? {
        rows: BigInt(job.rows),
        bytes: BigInt(job.rows * options.rowWidth),
        files: [`${job.outputPath}.partial`],
        durationMs: BigInt(Date.now() - job.startedMs),
        notes: ['Cancelled: partial output kept as .partial'],
      }
    : undefined;
  log('warn', `Cancelled by user${started ? ` after ${job.rows.toLocaleString('en-US')} rows` : ''}`, id, 'bulkdb::job');
  finish(job, 'cancelled', { result, error: { code: 'CANCELLED', message: 'Cancelled by user', detail: result ? `Partial output: ${result.files[0]}` : undefined } });
}

function resumeJob(id: string, acks: string[]): void {
  const job = state.jobs.get(id);
  if (!job) throw err('NOT_FOUND', `Job ${id} not found`);
  if (job.summary.state !== 'warned') throw err('INVALID', `Job ${id} is ${job.summary.state}, not warned`);
  for (const a of acks) job.acks.add(a);
  const report = job.summary.preflight;
  const unacked = (report?.warnings ?? []).filter((w) => !job.acks.has(w.code)).map((w) => w.code);
  if (unacked.length > 0) throw err('INVALID', `Warnings not acknowledged: ${unacked.join(', ')}`);
  log('info', `Warnings acknowledged: ${[...job.acks].join(', ')}`, id, 'bulkdb::preflight');
  beginRunning(job);
}

/**
 * Storage key of a table's exact count: the connection plus core's `history::table_key`
 * (`[db.][schema.]name`, empty parts skipped), so `{ schema: 'public', name: 't' }` and
 * `{ database: '', schema: 'public', name: 't' }` share one entry exactly as they do in core.
 */
function exactCountKey(connId: string, ref: TableRef): string {
  const parts = [ref.database, ref.schema].filter((p): p is string => !!p);
  return `${connId}\u0000${[...parts, ref.name].join('.')}`;
}

/** Like core `record_exact_count`: a newer count of the same table replaces the older one. */
function rememberExactCount(connId: string, ref: TableRef, count: ExactCount): void {
  state.exactCounts.set(exactCountKey(connId, ref), count);
}

/**
 * Like core `catalog_last_exact_count`: unknown connection -> NOT_FOUND, never counted ->
 * `null`. The returned `table` is the caller's (core's stored key is lossy).
 */
function lastExactCount(connId: string, ref: TableRef): ExactCount | null {
  const conn = findConn(connId);
  const stored = state.exactCounts.get(exactCountKey(conn.id, ref));
  return stored ? { ...stored, table: ref } : null;
}

function startExactCount(connId: string, ref: TableRef): string {
  const conn = findConn(connId);
  const info = mockFindTable(catalogIdFor(connId), ref);
  if (!info) throw err('NOT_FOUND', `Table ${ref.schema ?? ''}.${ref.name} not found`);
  const id = nextId('cnt');
  const job: MockJob = {
    summary: { id, kind: 'exact_count', state: 'pending', connId, connName: conn.name, label: `count ${ref.schema ?? ''}.${ref.name}`, startedAt: nowIso() },
    acks: new Set(),
    startedMs: Date.now(),
    rows: 0,
    totalRows: 0,
    outputPath: '',
    serverTicks: 0,
    tick: 0,
  };
  state.jobs.set(id, job);
  setState(job, 'pending');
  log('info', `SELECT count(*) FROM ${ref.schema ?? ''}.${ref.name}`, id, 'bulkdb::catalog');
  job.stepTimer = after(100, () => {
    job.runStartMs = Date.now();
    setState(job, 'running');
    job.stepTimer = after(options.exactCountMs, () => {
      const est = num(info.estimatedRows) ?? 0;
      const rows = Math.round(est * 1.0031);
      const exactCount: ExactCount = { table: { database: ref.database, schema: ref.schema ?? info.schema, name: ref.name }, rows: BigInt(rows), elapsedMs: BigInt(Date.now() - job.startedMs), at: nowIso() };
      log('info', `count(*) = ${rows.toLocaleString('en-US')}`, id, 'bulkdb::catalog');
      // Persisted before `job:done` goes out, as core records it before emitting the event.
      rememberExactCount(connId, ref, exactCount);
      finish(job, 'done', { exactCount });
    });
  });
  return id;
}

// ---- tunnel prompts ------------------------------------------------------------------------

function askPrompt(kind: PendingPrompt['kind'], promptId: string): Promise<boolean | string | undefined> {
  return new Promise((resolve) => {
    state.prompts.set(promptId, { kind, resolve });
  });
}

async function probe(conn: Connection, password?: string): Promise<ProbeResult> {
  const driver = state.drivers.find((d) => d.id === conn.driver);
  if (!driver?.available) throw err('DRIVER_MISSING', `${driver?.name ?? conn.driver} is not available`, driver?.detail);
  const steps: string[] = [];
  const started = Date.now();
  if (conn.ssh?.enabled) {
    if (!state.knownHosts.has(conn.ssh.host)) {
      const promptId = nextId('hk');
      const prompt: HostKeyPrompt = {
        promptId,
        host: `${conn.ssh.host}:${conn.ssh.port}`,
        fingerprint: 'SHA256:u3zR9mZfQ0Jq1Xo3cLhZk7TQHhV4oWcK3o1F2bR8ZyA',
        keyType: 'ED25519',
        message: `The authenticity of host '${conn.ssh.host} (10.20.0.4)' can't be established.\nED25519 key fingerprint is SHA256:u3zR9mZfQ0Jq1Xo3cLhZk7TQHhV4oWcK3o1F2bR8ZyA.\nAre you sure you want to continue connecting (yes/no/[fingerprint])?`,
      };
      log('info', `ssh: unknown host key for ${conn.ssh.host}`, undefined, 'bulkdb::tunnel');
      emit(EV.tunnelHostKey, prompt);
      const ok = await askPrompt('hostkey', promptId);
      if (ok !== true) throw err('TUNNEL', 'Host key rejected', `ssh ${conn.ssh.host}`);
      state.knownHosts.add(conn.ssh.host);
    }
    if (conn.ssh.auth === 'password' && !state.sshSecrets.has(conn.id)) {
      const promptId = nextId('pw');
      const prompt: SshPasswordPrompt = { promptId, host: conn.ssh.host, user: conn.ssh.user, kind: 'password' };
      emit(EV.tunnelPassword, prompt);
      const secret = await askPrompt('secret', promptId);
      if (typeof secret !== 'string' || secret.length === 0) throw err('TUNNEL', 'SSH authentication cancelled');
    }
    steps.push('tunnel');
    log('info', `ssh -N -L 127.0.0.1:0:${conn.host}:${conn.port} ${conn.ssh.user}@${conn.ssh.host}`, undefined, 'bulkdb::tunnel');
  }
  requireAuth(conn, password);
  if (password === 'wrong') throw err('AUTH', `password authentication failed for user "${conn.user}"`);
  await sleep(options.latencyMs * 4);
  rememberSecret(conn, password);
  steps.push('connect', 'probe');
  const server = serverInfo(conn);
  log('info', `Connected to ${conn.name}: ${server.driver} ${server.version}`, undefined, 'bulkdb::source');
  return {
    server,
    tunnelUsed: !!conn.ssh?.enabled,
    elapsedMs: BigInt(Date.now() - started + 180),
    recommended:
      conn.driver === 'sqlite'
        ? { fetchBatchRows: 65536 }
        : { workMem: '128MB', statementTimeoutMs: BigInt(3_600_000), lockTimeoutMs: BigInt(5_000), maxParallelWorkers: 0 },
    steps,
  };
}

// ---- command table -------------------------------------------------------------------------

type Args = Record<string, unknown>;
type Handler = (args: Args) => unknown | Promise<unknown>;

function str(args: Args, key: string): string {
  const v = args[key];
  if (typeof v !== 'string' || v.length === 0) throw err('INVALID', `Missing argument: ${key}`);
  return v;
}

function optStr(args: Args, key: string): string | undefined {
  const v = args[key];
  return typeof v === 'string' ? v : undefined;
}

/** The `connId` argument's connection, authenticated like a real catalog/query call. */
function authedConn(args: Args): Connection {
  const conn = findConn(str(args, 'connId'));
  requireAuth(conn);
  return conn;
}

/** Catalog id (see `catalogIdFor`) of the authenticated `connId` argument. */
function authedCatalog(args: Args): string {
  return catalogIdFor(authedConn(args).id);
}

function tableArg(args: Args): TableRef {
  const v = args.table as TableRef | undefined;
  if (!v || typeof v.name !== 'string') throw err('INVALID', 'Missing argument: table');
  return v;
}

const handlers: Record<string, Handler> = {
  drivers_list: () => state.drivers,
  drivers_rescan: () => {
    state.drivers = state.drivers.map((d) =>
      !d.available && state.installedDrivers.has(d.id) ? { ...d, available: true, detail: `${d.name} client found (mock)` } : d,
    );
    log('info', `Driver rescan: ${state.drivers.filter((d) => d.available).length} available`, undefined, 'bulkdb::drivers');
    emit(EV.driversChanged, state.drivers);
    return state.drivers;
  },

  conn_list: () => state.connections,
  conn_get: (a): Connection => {
    const c = findConn(str(a, 'connId'));
    // Like core `with_secret_flags`: the flags are read from the "keyring" on every call.
    return { ...c, hasPassword: state.passwords.has(c.id), hasSshSecret: state.sshSecrets.has(c.id) };
  },
  conn_save: (a) => {
    const input = a.input as ConnectionInput | undefined;
    if (!input) throw err('INVALID', 'Missing argument: input');
    if (!input.name?.trim()) throw err('INVALID', 'Connection name is required');
    if (!state.drivers.some((d) => d.id === input.driver)) throw err('INVALID', `Unknown driver ${input.driver}`);
    // Like core `conn_save`: clearing a secret and giving a new one in the same save contradict.
    if (input.clearPassword && input.password) throw err('INVALID', 'cannot clear the stored password: a new password was given in the same save');
    if (input.clearSshSecret && (input.sshPassword || input.sshKeyPassphrase)) {
      throw err('INVALID', 'cannot clear the stored SSH secret: a new SSH password or key passphrase was given in the same save');
    }
    const id = input.id || nextId('conn');
    const existing = state.connections.find((c) => c.id === id);
    if (input.savePassword) {
      if (input.password) state.passwords.set(id, input.password);
    } else {
      state.passwords.delete(id);
    }
    if (input.sshPassword) state.sshSecrets.set(id, input.sshPassword);
    if (input.sshKeyPassphrase) state.sshSecrets.set(id, input.sshKeyPassphrase);
    // Each flag removes exactly its own secret.
    if (input.clearPassword) {
      state.passwords.delete(id);
      state.sessionSecrets.delete(id);
    }
    if (input.clearSshSecret) state.sshSecrets.delete(id);
    const conn: Connection = {
      id,
      name: input.name.trim(),
      driver: input.driver,
      host: input.host,
      port: input.port,
      database: input.database,
      user: input.user,
      fields: { ...input.fields },
      ssh: input.ssh,
      session: { ...input.session },
      outputDir: input.outputDir,
      savePassword: input.savePassword,
      hasPassword: state.passwords.has(id),
      hasSshSecret: state.sshSecrets.has(id),
    };
    if (existing) state.connections = state.connections.map((c) => (c.id === id ? conn : c));
    else state.connections = [...state.connections, conn];
    log('info', `Connection ${existing ? 'updated' : 'created'}: ${conn.name} (password ${conn.hasPassword ? 'in keyring' : 'not stored'})`, undefined, 'bulkdb::config');
    return conn;
  },
  conn_delete: (a) => {
    const id = str(a, 'connId');
    findConn(id);
    state.connections = state.connections.filter((c) => c.id !== id);
    state.passwords.delete(id);
    state.sessionSecrets.delete(id);
    state.sshSecrets.delete(id);
    log('info', `Connection deleted: ${id}`, undefined, 'bulkdb::config');
  },
  conn_test: (a) => probe(findConn(str(a, 'connId')), optStr(a, 'password')),
  conn_import_dbeaver: (a): ImportedConnection[] => {
    const path = str(a, 'path');
    if (!/\.json$/i.test(path)) throw err('INVALID', `not a DBeaver data-sources.json: ${path}`);
    if (/missing/i.test(path)) throw err('IO', `${path}: no such file`);
    const base = { fields: {}, session: {}, savePassword: true, hasPassword: false, hasSshSecret: false };
    // Like core `dbeaver::parse`: every connection becomes a row (unsupported ones with a note),
    // sorted by name; nothing is saved until `conn_save`.
    return [
      {
        connection: { ...base, id: 'dbeaver-ora1', name: 'erp-oracle (DBeaver)', driver: 'oracle', host: 'erp-db.internal', port: 1521, user: 'erp_ro', fields: { service_name: 'ERP' } },
        notes: [],
      },
      {
        connection: { ...base, id: 'dbeaver-ch1', name: 'events-clickhouse (DBeaver)', driver: 'clickhouse', host: 'ch.internal', port: 8123 },
        notes: ["unsupported provider 'clickhouse' (DBeaver driver 'com_clickhouse')"],
      },
      {
        connection: { ...base, id: 'dbeaver-my1', name: 'staging-mysql (DBeaver)', driver: 'mysql', host: 'mysql-staging.internal', port: 3306, database: 'shop', user: 'reporter', fields: { sslmode: 'prefer' } },
        notes: [],
      },
    ];
  },

  catalog_databases: (a): DatabaseInfo[] => mockDatabases(authedCatalog(a)) ?? [],
  catalog_schemas: (a): SchemaInfo[] => {
    const id = authedCatalog(a);
    const s = mockSchemas(id, str(a, 'db'));
    if (!s) throw err('NOT_FOUND', `Database ${String(a.db)} not found`);
    return s;
  },
  catalog_tables: (a): TableInfo[] => {
    const id = authedCatalog(a);
    const t = mockTables(id, str(a, 'db'), str(a, 'schema'));
    if (!t) throw err('NOT_FOUND', `Schema ${String(a.db)}.${String(a.schema)} not found`);
    return t;
  },
  catalog_columns: (a): ColumnInfo[] => {
    const id = authedCatalog(a);
    const ref = tableArg(a);
    if (!mockFindTable(id, ref)) throw err('NOT_FOUND', `Table ${ref.schema ?? ''}.${ref.name} not found`);
    return mockColumns(id, ref);
  },
  catalog_indexes: (a): IndexInfo[] => mockIndexes(authedCatalog(a), tableArg(a)),
  catalog_ddl: (a): string => mockDdl(authedCatalog(a), tableArg(a)),
  catalog_select_sql: (a): string => mockSelectSql(authedCatalog(a), tableArg(a)),
  catalog_exact_count: (a): string => startExactCount(authedConn(a).id, tableArg(a)),
  // Local state only (core reads its history database): no password is needed.
  catalog_last_exact_count: (a): ExactCount | null => lastExactCount(str(a, 'connId'), tableArg(a)),
  catalog_invalidate: (a) => {
    // Like core, any id is accepted (nothing cached = nothing to drop). The mock serves canned
    // metadata, so there is no cache; the call is only logged.
    const connId = str(a, 'connId');
    log('debug', `Catalog cache cleared: ${connId}`, undefined, 'bulkdb::catalog');
  },

  query_explain: async (a): Promise<PlanSummary> => {
    const conn = authedConn(a);
    // Like core: the guard reads the query with the connection's dialect and only the
    // normalized statement is explained; a refusal is INVALID.
    const check = mockSqlCheck(str(a, 'sql'), mockDialect(conn.driver));
    if (!check.ok) throw err('INVALID', check.issues.map((i) => i.message).join(' '), check.issues[0]?.code);
    await sleep(options.latencyMs * 3);
    return buildPlan(conn.id, check.normalized, a.analyze === true);
  },
  sql_check: (a): SqlCheck => {
    const connId = optStr(a, 'connId');
    const dialect = connId ? mockDialect(findConn(connId).driver) : 'standard';
    return mockSqlCheck(typeof a.sql === 'string' ? a.sql : '', dialect);
  },
  sql_file_read: (a): SqlFile => {
    const path = str(a, 'path');
    if (/missing/i.test(path)) throw err('NOT_FOUND', `${path}: no such file`);
    if (!/\.sql$/i.test(path)) throw err('IO', `${path} is not a .sql file`);
    const content = mockSqlFileContent(path);
    return { path, content, modifiedAt: nowIso(), sizeBytes: BigInt(content.length) };
  },

  export_preflight: (a): PreflightReport => {
    const req = a.req as ExportRequest | undefined;
    if (!req) throw err('INVALID', 'Missing argument: req');
    if (!req.sql?.trim() && !req.sqlFile) throw err('INVALID', 'Either sql or sqlFile must be set');
    const conn = findConn(req.connId);
    requireAuth(conn, req.password);
    rememberSecret(conn, req.password);
    return buildPreflight(conn, req);
  },
  export_start: (a): string => {
    const req = a.req as ExportRequest | undefined;
    if (!req) throw err('INVALID', 'Missing argument: req');
    const conn = findConn(req.connId);
    requireAuth(conn, req.password);
    rememberSecret(conn, req.password);
    // Like core: the stored request (what `job_get` returns) never carries the password.
    return startExportJob({ ...req, password: undefined }, (a.acknowledgedWarnings as string[] | undefined) ?? []);
  },
  job_resume: (a) => resumeJob(str(a, 'jobId'), (a.acknowledgedWarnings as string[] | undefined) ?? []),
  job_cancel: (a) => cancelJob(str(a, 'jobId')),
  job_list: (): JobSummary[] => [...state.jobs.values()].map((j) => j.summary),
  job_get: (a): JobSummary => {
    const j = state.jobs.get(str(a, 'jobId'));
    if (!j) throw err('NOT_FOUND', `Job ${String(a.jobId)} not found`);
    return j.summary;
  },

  history_list: (a): HistoryEntry[] => {
    const connId = optStr(a, 'connId');
    const limit = typeof a.limit === 'number' ? a.limit : 200;
    return state.history.filter((h) => !connId || h.connId === connId).slice(0, limit);
  },
  settings_get: (): Settings => state.settings,
  settings_set: (a): Settings => {
    const s = a.settings as Settings | undefined;
    if (!s) throw err('INVALID', 'Missing argument: settings');
    if (s.maxConcurrentJobs < 1 || s.maxConcurrentJobs > 3) throw err('INVALID', 'maxConcurrentJobs must be 1..3');
    state.settings = { ...state.settings, ...s, ui: { ...s.ui } };
    log('info', 'Settings saved', undefined, 'bulkdb::config');
    return state.settings;
  },

  tunnel_confirm_hostkey: (a) => {
    const id = str(a, 'promptId');
    const p = state.prompts.get(id);
    if (!p || p.kind !== 'hostkey') throw err('NOT_FOUND', `Prompt ${id} not pending`);
    state.prompts.delete(id);
    p.resolve(a.accept === true);
  },
  tunnel_provide_secret: (a) => {
    const id = str(a, 'promptId');
    const p = state.prompts.get(id);
    if (!p || p.kind !== 'secret') throw err('NOT_FOUND', `Prompt ${id} not pending`);
    state.prompts.delete(id);
    p.resolve(optStr(a, 'secret'));
  },

  logs_recent: (a): LogEvent[] => {
    const limit = typeof a.limit === 'number' ? a.limit : 500;
    return state.logs.slice(-limit);
  },
  diagnostics_bundle: (a): DiagnosticsBundle => {
    const outDir = str(a, 'outDir');
    const path = joinPath(outDir, `bulkdb-diagnostics-${nowIso().slice(0, 10)}.zip`);
    log('info', `Diagnostics bundle written: ${path}`, undefined, 'bulkdb::diagnostics');
    return { path, included: ['app.log', 'config.json (masked)', 'drivers.json', 'history.sqlite', 'system.txt'], sizeBytes: BigInt(184_320) };
  },
  app_info: (): AppInfo => ({
    version: '0.1.0-mock',
    platform: 'browser',
    arch: 'wasm32',
    configDir: '(in-browser mock)',
    logDir: '(in-browser mock)',
  }),
  open_path: (a) => {
    log('info', `open_path ${str(a, 'path')} (no file manager in the browser mock)`, undefined, 'bulkdb::mock');
  },
};

/** Dispatch one command. Unknown commands and handler errors reject with `IpcError`. */
export async function mockInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  calls.push(cmd);
  const h = handlers[cmd];
  if (!h) throw err('INTERNAL', `Unknown command: ${cmd}`);
  await sleep(options.latencyMs);
  try {
    return (await h(args ?? {})) as T;
  } catch (e) {
    if (e instanceof IpcError) throw e;
    throw err('INTERNAL', e instanceof Error ? e.message : String(e));
  }
}

function seedLogs(): void {
  log('info', 'BulkDB mock backend ready (not running inside Tauri)', undefined, 'bulkdb::mock');
  log('info', `Drivers: ${state.drivers.filter((d) => d.available).map((d) => d.id).join(', ')} available; ${state.drivers.filter((d) => !d.available).map((d) => d.id).join(', ')} missing`, undefined, 'bulkdb::drivers');
  log('debug', 'Config loaded from %APPDATA%\\bulkdb\\config.json (mock)', undefined, 'bulkdb::config');
}

/** Test / demo controls for the fake backend. */
export const mockBackend = {
  /** Command names in invocation order (cleared by `reset()`). */
  calls,
  get options(): MockOptions {
    return options;
  },
  /** Override timings (e.g. `latencyMs: 0` in tests). */
  configure(partial: Partial<MockOptions>): void {
    options = { ...options, ...partial };
  },
  /** Clear all timers, listeners, state and recorded calls; restore default options. */
  reset(): void {
    for (const h of timers) {
      clearTimeout(h);
      clearInterval(h);
    }
    timers.clear();
    listeners.clear();
    for (const p of state.prompts.values()) p.resolve(undefined);
    state = freshState();
    options = { ...DEFAULT_OPTIONS };
    calls.length = 0;
    seedLogs();
  },
  /** Push an event to listeners as the shell would. */
  emit(event: string, payload: unknown): void {
    emit(event, payload);
  },
  getJob(id: string): JobSummary | undefined {
    return state.jobs.get(id)?.summary;
  },
  /** Simulate installing a vendor client: the driver becomes available on the next `drivers_rescan`. */
  installDriver(id: string): void {
    state.installedDrivers.add(id);
  },
  pendingPrompts(): string[] {
    return [...state.prompts.keys()];
  },
  hostKnown(host: string): boolean {
    return state.knownHosts.has(host);
  },
  sqlCheck: mockSqlCheck,
  /**
   * Store an exact count as if a `catalog_exact_count` job had finished earlier (in an
   * earlier session, say), so `catalog_last_exact_count` returns it.
   */
  seedExactCount(connId: string, count: ExactCount): void {
    rememberExactCount(connId, count.table, count);
  },
  /** Secrets "in the keyring" for a connection (tests of the clear-secret flags). */
  storedSecrets(connId: string): { password: boolean; sessionPassword: boolean; sshSecret: boolean } {
    return { password: state.passwords.has(connId), sessionPassword: state.sessionSecrets.has(connId), sshSecret: state.sshSecrets.has(connId) };
  },
};

seedLogs();
