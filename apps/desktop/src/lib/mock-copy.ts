// In-browser fake of the DB copy commands (docs/db-copy.md 6.2), owner copy-ui.
//
// Plans are built from the same canned catalog the browser mock serves (`mock-data`), so the
// wizard's object tree and the plan agree: PostgreSQL 15 `prod` → 16 `staging` (every table
// already exists in `staging/orders`, so the default **skip** policy and its acknowledgement show
// up), the legacy MySQL 4.0 EUC-KR server (`shop`, `board`: MyISAM, `timestamp(14)`, zero
// dates, Korean ENUM values) → the MySQL 8.0 demo target `mysql8` or any other MySQL connection
// (8.0 by default; a host naming 5.5 / 5.7 / 8.4 / 9 / ndb picks that version), and SQLite →
// SQLite. A database the fixtures do not know plans two plain tables (`customers`, `orders`).
//
// Runs are timed (`progressTickMs` × ${COPY_TICK_FACTOR}, about 20-30 s with the demo defaults so the
// progress view can be watched): pre-data, data with `tableConcurrency` tables at once,
// post-data, finalize, verify. `copy:progress` carries deltas ordered by `seq`. Cancel keeps a
// resumable result with what stays in the target; `copy_resume` re-plans the run with its
// checkpoints (`resume_done`, reloads) as a new job of the same run.
//
// Like core (5.1), `copy_start` / `copy_resume` return a job at once; the job plans again in
// `preflight` and, when acknowledgements are missing or the plan changed since it was reviewed
// (`PLAN_CHANGED`, with the changed item ids), waits in `warned` with one report warning per
// missing key until `job_resume(jobId, keys)` (`MockCtx.jobs.onResume`). Blockers of that plan
// end the job `rejected` without a result (the plan stays in `copy_status`), its reason only in
// the job's error (`JobDone.error`: INVALID, "the copy plan has blockers: …", one "CODE: message"
// line per blocker), as core's `End::Rejected`. A run keeps its last result when a later job
// ends without one (`copy_run`). Jobs start with `tables_total` 0 until planned, as in core.
// Deviation: resuming a run that is in use still fails the call (RUN_IN_USE) instead of a job.

import type {
  AckNeed,
  ColumnInfo,
  Connection,
  CopyEffective,
  CopyFamily,
  CopyIssue,
  CopyItemResult,
  CopyItemState,
  CopyJobInfo,
  CopyPhase,
  CopyPlan,
  CopyProgress,
  CopyRequest,
  CopyResult,
  CopyResumeRequest,
  CopyRunDetail,
  CopyRunInfo,
  CopyServerFacts,
  CopyStatus,
  CopyTableProgress,
  CopyTableResult,
  ExistingTablePolicy,
  JobProgress,
  JobState,
  LeftBehind,
  LoadMethod,
  PlanAction,
  PreflightReport,
  PlanItem,
  ResumePartial,
  Severity,
  TableKind,
  ValueFix,
} from 'ipc-types';
import { LOCAL_CONN_ID, mockCatalogId, mockColumns, mockDatabases, mockDdl, mockIndexes, mockMysqlIsLegacy, mockSchemas, mockTables } from './mock-data';
import type { MockArgs, MockCtx, MockFeature } from './mock-ipc';

// ---- small helpers ---------------------------------------------------------------------------

const ACTIVE: readonly JobState[] = ['pending', 'preflight', 'warned', 'running', 'finishing'];
/** Issues that follow live state and are not part of `plan_id` (section 5.3 "env"). */
const ENV_CODES = new Set([
  'TARGET_NOT_WRITABLE',
  'PRODUCTION_CONFIRM',
  'WORK_MEM_HIGH',
  'CONCURRENT_SESSION',
  'SRC_MYISAM_LOCK',
  'SRC_LONG_SNAPSHOT',
  'DISK_LOW',
  'RUN_IN_USE',
  'TARGET_IN_USE',
  'SOURCE_BEING_WRITTEN',
]);
const LOADS: readonly PlanAction[] = ['create', 'truncate', 'drop_recreate', 'append'];
const GENERIC: readonly [string, number][] = [
  ['customers', 1_200],
  ['orders', 48_000],
];
const ROW_BYTES = 96;
/** Data ticks of a whole run at concurrency 1 (spread over the tables by size). */
const DATA_TICKS = 30;
/** A copy tick is this many `progressTickMs`: about 40 ticks × 600 ms with the demo defaults. */
const COPY_TICK_FACTOR = 3;
/** Plans kept for `PLAN_CHANGED` details (core keeps recent plans the same way). */
const RECENT_PLANS = 32;

/** FNV-1a over the UTF-16 code units, `lanes` x 32 bits as hex. Stable, not cryptographic. */
function fnv(text: string, lanes: number): string {
  let out = '';
  for (let lane = 0; lane < lanes; lane += 1) {
    let h = 0x811c9dc5 ^ lane;
    for (let i = 0; i < text.length; i += 1) {
      h ^= text.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    out += h.toString(16).padStart(8, '0');
  }
  return out;
}

const hashNum = (text: string): number => parseInt(fnv(text, 1), 16);
/** Double-quoted, `"` doubled: the path parts of plan item ids. */
const dq = (name: string): string => `"${name.replace(/"/g, '""')}"`;
const bq = (name: string): string => `\`${name.replace(/`/g, '``')}\``;

function familyOf(driver: string): CopyFamily | null {
  return driver === 'postgres' || driver === 'mysql' || driver === 'sqlite' ? driver : null;
}

function issue(code: string, severity: Severity, message: string, extra: Partial<CopyIssue> = {}): CopyIssue {
  return { code, severity, message, ...extra };
}

// ---- servers ---------------------------------------------------------------------------------

interface Server {
  driver: string;
  version: string;
  major: number;
  minor: number;
  patch: number;
  legacy: boolean;
  localInfile: boolean;
  charset?: string;
  collation?: string;
  notes: string[];
}

function parseVersion(v: string): [number, number, number] {
  const m = /^(\d+)\.(\d+)(?:\.(\d+))?/.exec(v);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3] ?? 0)] : [0, 0, 0];
}

const atLeast = (s: Server, major: number, minor = 0, patch = 0): boolean =>
  s.major !== major ? s.major > major : s.minor !== minor ? s.minor > minor : s.patch >= patch;

/** What the fake servers report, by connection (host names pick versions). */
function serverOf(conn: Connection): Server {
  const host = conn.host ?? '';
  let version: string;
  let legacy = false;
  if (conn.driver === 'postgres') {
    version = /17/.test(host) ? '17.2' : /16|staging/.test(host) ? '16.4' : '15.8';
  } else if (conn.driver === 'mysql') {
    legacy = mockMysqlIsLegacy(conn);
    version = legacy
      ? '4.0.27-standard-log'
      : /ndb/i.test(host)
        ? '5.7.44-ndb-7.6.29'
        : /mysql-?9|my9/i.test(host)
          ? '9.1.0'
          : /8\.?4/.test(host)
            ? '8.4.3'
            : /5\.?7/.test(host)
              ? '5.7.44'
              : /5\.?5/.test(host)
                ? '5.5.62'
                : '8.0.36';
  } else {
    version = '3.46.0';
  }
  const [major, minor, patch] = parseVersion(version);
  const s: Server = { driver: conn.driver, version, major, minor, patch, legacy, localInfile: false, notes: [] };
  if (conn.driver === 'postgres') {
    s.charset = 'UTF8';
    s.collation = major >= 17 && /17/.test(host) ? 'C.UTF-8 (builtin)' : 'en_US.utf8';
  } else if (conn.driver === 'mysql') {
    if (legacy) {
      s.charset = 'euckr';
      s.collation = 'euckr_korean_ci';
      s.notes.push('pre-4.1 protocol: text decoded as EUC-KR (CP949)', 'MyISAM tables: no transactional snapshot');
    } else {
      s.localInfile = major < 8;
      s.charset = atLeast(s, 5, 5, 3) ? 'utf8mb4' : 'utf8';
      s.collation = major >= 8 ? 'utf8mb4_0900_ai_ci' : 'utf8mb4_general_ci';
      s.notes.push(`local_infile = ${s.localInfile ? 'ON' : 'OFF'}`);
      if (major >= 8) s.notes.push('sql_mode = ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION');
    }
  }
  return s;
}

function endpointOf(conn: Connection): string {
  if (conn.driver === 'sqlite') return conn.fields.path ?? '';
  const hp = `${conn.host ?? 'localhost'}:${conn.port ?? 0}`;
  return conn.ssh?.enabled && conn.ssh.host ? `${hp} via ssh ${conn.ssh.host}` : hp;
}

function factsOf(conn: Connection, s: Server, database: string): CopyServerFacts {
  return {
    driver: conn.driver,
    version: s.version,
    database,
    endpoint: endpointOf(conn),
    ...(s.charset ? { charset: s.charset } : {}),
    ...(s.collation ? { collation: s.collation } : {}),
    ...(conn.user ? { user: conn.user } : {}),
    notes: [...s.notes],
  };
}

// ---- catalog (the same fixtures the browser mock browses) ----------------------------------

const catalogId = (conn: Connection): string => mockCatalogId(conn);

interface SrcTable {
  schema: string;
  name: string;
  rows: number;
  bytes: number;
  kind: TableKind;
}

/** Source tables of the endpoint, or null when the fixtures do not know that database. */
function sourceTables(conn: Connection, family: CopyFamily, db: string, schema: string | undefined, legacy: boolean): SrcTable[] | null {
  const cid = catalogId(conn);
  const schemas = mockSchemas(cid, db);
  if (!schemas) return null;
  const names = schemas.map((s) => s.name).filter((n) => family !== 'postgres' || !schema || n === schema);
  const seen = new Set<string>();
  const out: SrcTable[] = [];
  for (const s of names) {
    for (const t of mockTables(cid, db, s) ?? []) {
      if (family !== 'postgres' && seen.has(t.name)) continue;
      if (family === 'mysql' && legacy && (t.kind === 'view' || t.kind === 'materialized_view')) continue;
      seen.add(t.name);
      out.push({ schema: s, name: t.name, rows: Number(t.estimatedRows ?? 0), bytes: Number(t.totalBytes ?? 0), kind: t.kind });
    }
  }
  return out;
}

interface TargetView {
  dbExists: boolean;
  schemas: Set<string>;
  tables: Map<string, { hasRows: boolean; rows: number }>;
}

function targetView(conn: Connection, family: CopyFamily, db: string): TargetView {
  const cid = catalogId(conn);
  const dbExists = family === 'sqlite' || (mockDatabases(cid) ?? []).some((d) => d.name === db);
  const schemas = new Set<string>();
  const tables = new Map<string, { hasRows: boolean; rows: number }>();
  if (!dbExists) return { dbExists, schemas, tables };
  for (const s of mockSchemas(cid, db) ?? []) {
    schemas.add(s.name);
    for (const t of mockTables(cid, db, s.name) ?? []) {
      const key = family === 'postgres' ? `${s.name}.${t.name}` : t.name;
      // Row presence is an exact probe in core; the fixtures make every fourth table empty.
      const hasRows = hashNum(`${db}.${key}`) % 4 !== 0;
      if (!tables.has(key)) tables.set(key, { hasRows, rows: hasRows ? Number(t.estimatedRows ?? 0) : 0 });
    }
  }
  return { dbExists, schemas, tables };
}

// ---- runs and checkpoints ----------------------------------------------------------------------

type CheckpointState = 'pending' | 'created' | 'loading' | 'done' | 'failed' | 'skipped';

interface Checkpoint {
  state: CheckpointState;
  createdByRun: boolean;
  rows: number;
}

interface MockRun {
  runId: string;
  label: string;
  /** As stored: no passwords, no start-time fields. */
  req: CopyRequest;
  dryRun: boolean;
  checkpoints: Map<string, Checkpoint>;
  jobId: string;
  startedAt: string;
  finishedAt?: string;
  plan: CopyPlan;
  /** The last result of the run (core keeps `result_json` when a later job ends without one). */
  result?: CopyResult;
}

interface ResumeState {
  runId: string;
  checkpoints: Map<string, Checkpoint>;
  partial: ResumePartial;
}

// ---- planning ----------------------------------------------------------------------------------

interface PlanInput {
  req: CopyRequest;
  source: Connection;
  target: Connection;
  resume?: ResumeState;
  /** At start / resume: `PRODUCTION_CONFIRM` is a blocker unless the name matches. */
  atStart: boolean;
  /** Other runs, for `TARGET_IN_USE`. */
  busyTargets: (connId: string, db: string, exceptRun?: string) => boolean;
}

interface Builder {
  items: PlanItem[];
  warnings: CopyIssue[];
  blockers: CopyIssue[];
  order: Record<CopyPhase, number>;
}

function newItem(b: Builder, p: Omit<PlanItem, 'order' | 'warnings' | 'dependsOn' | 'targetSql' | 'existsInTarget' | 'hasData'> & Partial<PlanItem>): PlanItem {
  const it: PlanItem = { existsInTarget: false, hasData: false, targetSql: [], warnings: [], dependsOn: [], ...p, order: b.order[p.phase]++ };
  b.items.push(it);
  return it;
}

/** The action of a child object (index, key, sequence value, analyze) of a table with `parent` action. */
function childAction(parent: PlanAction, existingObject: boolean, runs: boolean): PlanAction {
  if (parent === 'excluded' || parent === 'skip_existing' || parent === 'resume_done' || parent === 'unsupported') return parent;
  if ((parent === 'truncate' || parent === 'append') && existingObject && !runs) return 'skip_existing';
  return runs ? 'run' : 'create';
}

interface TableDecision {
  action: PlanAction;
  exists: boolean;
  hasRows?: boolean;
  targetRows?: number;
  issues: CopyIssue[];
  blocker?: CopyIssue;
}

function decideTable(inp: PlanInput, id: string, exists: { hasRows: boolean; rows: number } | undefined, label: string): TableDecision {
  const { req, resume } = inp;
  const o = req.options;
  const policy: ExistingTablePolicy = o.existingOverrides.find((x) => x.itemId === id)?.policy ?? o.existing;
  const issues: CopyIssue[] = [];
  const cp = resume?.checkpoints.get(id);
  const isExisting = !!exists || !!cp?.createdByRun;
  const base = { exists: isExisting, hasRows: exists ? exists.hasRows : cp ? cp.rows > 0 : undefined, targetRows: exists?.rows };
  if (o.exclude.includes(id)) return { ...base, action: 'excluded', issues };
  if (cp) {
    if (cp.state === 'done' || cp.state === 'skipped') {
      issues.push(issue('RESUME_DONE', 'info', `${label} finished in this run`, { itemId: id }));
      return { ...base, action: 'resume_done', issues };
    }
    if (cp.state !== 'pending') {
      if (cp.createdByRun || policy === 'truncate' || policy === 'drop_recreate') {
        issues.push(issue('RESUME_RELOAD', 'info', `${label} is emptied and loaded again`, { itemId: id, detail: `${cp.rows} rows were written before` }));
        return { ...base, exists: true, action: 'truncate', issues };
      }
      if (policy === 'append' && cp.rows > 0) {
        if (resume?.partial === 'truncate') {
          issues.push(issue('DESTRUCTIVE_TRUNCATE', 'danger', `${label}: the table is emptied before loading again`, { itemId: id }));
          return { ...base, exists: true, action: 'truncate', issues };
        }
        if (resume?.partial === 'skip') {
          issues.push(issue('RESUME_DONE', 'info', `${label} is left as it is (partly appended)`, { itemId: id }));
          return { ...base, action: 'resume_done', issues };
        }
        return {
          ...base,
          action: 'append',
          issues,
          blocker: issue('RESUME_APPEND_PARTIAL', 'danger', `${label} was partly appended (${cp.rows} rows)`, {
            itemId: id,
            recommendation: 'Resume with partial = truncate (empty and reload) or skip (leave it)',
          }),
        };
      }
    }
  }
  if (!isExisting) {
    if (o.content === 'data_only') {
      return {
        ...base,
        action: 'create',
        issues,
        blocker: issue('TABLE_MISSING', 'danger', `${label} does not exist in the target (data only)`, { itemId: id, recommendation: 'Choose "structure and data" or exclude the table' }),
      };
    }
    return { ...base, action: 'create', issues };
  }
  switch (policy) {
    case 'truncate':
      if (base.hasRows) {
        issues.push(issue('DESTRUCTIVE_TRUNCATE', 'danger', `${label} holds data that is deleted before loading`, { itemId: id }));
        if (inp.target.driver === 'postgres') {
          issues.push(issue('TARGET_LOCKED_DURING_LOAD', 'warning', `${label} stays locked (ACCESS EXCLUSIVE) until its load commits`, { itemId: id }));
        }
      }
      return { ...base, action: 'truncate', issues };
    case 'drop_recreate':
      if (base.hasRows) issues.push(issue('DESTRUCTIVE_DROP', 'danger', `${label} holds data and is dropped`, { itemId: id }));
      return { ...base, action: 'drop_recreate', issues };
    case 'append':
      return { ...base, action: 'append', issues };
    default:
      return { ...base, action: 'skip_existing', issues };
  }
}

function pgType(c: ColumnInfo): string {
  return c.dataType;
}

function pgDefault(c: ColumnInfo): string {
  return c.default ? ` DEFAULT ${c.default}` : '';
}

interface MysqlColumn {
  name: string;
  src: string;
  tgt: string;
  zeroDate?: boolean;
  decimal?: boolean;
  timestampFirst?: boolean;
  /** ENUM values that differ only by case (they collide under a `_ci` collation). */
  enumCaseDuplicate?: boolean;
}

/** A DEFAULT clause; `CURRENT_TIMESTAMP` stays a keyword, anything else is quoted. */
function defaultClause(def: string | undefined, kw: string): string {
  if (def === undefined) return '';
  return /^CURRENT_TIMESTAMP/i.test(def) ? ` ${kw} ${def}` : ` ${kw} '${def.replace(/'/g, "''")}'`;
}

/**
 * The columns of a fixture MySQL table (types as `SHOW COLUMNS` prints them) and their
 * translation for `tgt` (section 3.3, simplified): integer display widths go from 8.0.17, the
 * first 4.0 `timestamp(n)` keeps its automatic initialisation and update, the other 4.0
 * TIMESTAMPs lose them, zero dates follow the policy, 4.0 DECIMALs gain a digit.
 */
function mysqlColumns(cols: ColumnInfo[], tgt: Server, zero: 'null' | 'keep' | 'min', legacy: boolean): MysqlColumn[] {
  const noWidth = atLeast(tgt, 8, 0, 17);
  const srcKw = legacy ? 'default' : 'DEFAULT';
  let firstTs = legacy;
  return cols.map((c): MysqlColumn => {
    const t = c.dataType.toLowerCase();
    const notNull = c.nullable ? '' : ' NOT NULL';
    const srcDef = defaultClause(c.default, srcKw);
    const tgtDef = defaultClause(c.default, 'DEFAULT');
    const same = { name: c.name, src: `${c.dataType}${notNull}${srcDef}`, tgt: `${c.dataType}${notNull}${tgtDef}` };
    let m: RegExpExecArray | null;
    if ((m = /^(tinyint|smallint|mediumint|int|bigint)(\(\d+\))?( unsigned)?$/.exec(t))) {
      const tgtType = `${m[1]}${noWidth ? '' : (m[2] ?? '')}${m[3] ?? ''}`;
      if (c.primaryKey && c.position === 1) return { name: c.name, src: `${c.dataType} NOT NULL ${legacy ? 'auto_increment' : 'AUTO_INCREMENT'}`, tgt: `${tgtType} NOT NULL AUTO_INCREMENT` };
      return { ...same, tgt: `${tgtType}${notNull}${tgtDef}` };
    }
    if (t.startsWith('timestamp')) {
      if (firstTs) {
        firstTs = false;
        return { name: c.name, src: `${c.dataType} NOT NULL`, tgt: 'timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP', timestampFirst: true };
      }
      return legacy ? { name: c.name, src: `${c.dataType}${notNull}`, tgt: 'timestamp NULL DEFAULT NULL' } : same;
    }
    if ((t === 'datetime' || t === 'date') && c.default !== undefined && /^0000-00-00/.test(c.default)) {
      const min = t === 'date' ? '1000-01-01' : '1000-01-01 00:00:00';
      const tgtType = zero === 'null' ? `${t} NULL DEFAULT NULL` : zero === 'min' ? `${t} NOT NULL DEFAULT '${min}'` : `${t} NOT NULL DEFAULT '${c.default}'`;
      return { name: c.name, src: same.src, tgt: tgtType, zeroDate: true };
    }
    if ((m = /^decimal\((\d+),(\d+)\)/.exec(t)) && legacy) {
      return { name: c.name, src: same.src, tgt: `decimal(${Number(m[1]) + 1},${m[2]})${notNull}${tgtDef}`, decimal: true };
    }
    if (t.startsWith('enum(')) {
      const values = [...c.dataType.matchAll(/'((?:[^']|'')*)'/g)].map((x) => x[1].toLowerCase());
      return { ...same, enumCaseDuplicate: new Set(values).size < values.length };
    }
    return same;
  });
}

interface EffectiveMysql {
  charset: string;
  collation: string;
  engine: string;
  zero: 'null' | 'keep' | 'min';
}

function mysqlEffective(req: CopyRequest, src: Server, tgt: Server): EffectiveMysql {
  const m = req.options.mysql;
  const charset = m.charset ?? (atLeast(tgt, 5, 5, 3) ? 'utf8mb4' : 'utf8');
  const defaultCollation = charset === 'utf8mb4' ? (tgt.major >= 8 ? 'utf8mb4_0900_ai_ci' : 'utf8mb4_general_ci') : `${charset}_general_ci`;
  const collation = m.collation === 'legacy' ? (atLeast(tgt, 5, 6) ? 'utf8mb4_unicode_520_ci' : 'utf8mb4_general_ci') : (m.collation ?? defaultCollation);
  const engine = m.engine === 'myisam' || (m.engine === 'keep' && src.legacy) ? 'MyISAM' : 'InnoDB';
  const zero = m.zeroDates === 'auto' ? (atLeast(tgt, 5, 7) ? 'null' : 'keep') : m.zeroDates;
  return { charset, collation, engine, zero };
}

function buildPlan(inp: PlanInput): CopyPlan {
  const started = Date.now();
  const { req, source, target } = inp;
  const o = req.options;
  const family = familyOf(source.driver);
  const src = serverOf(source);
  const tgt = serverOf(target);
  const b: Builder = { items: [], warnings: [], blockers: [], order: { plan: 0, pre_data: 0, data: 0, post_data: 0, finalize: 0, verify: 0 } };

  // ---- endpoint blockers (5.3)
  if (!family) b.blockers.push(issue('FAMILY_UNSUPPORTED', 'danger', `DB copy supports postgres, mysql and sqlite, not ${source.driver}`));
  else if (target.driver !== source.driver) b.blockers.push(issue('FAMILY_MISMATCH', 'danger', `source is ${source.driver}, target is ${target.driver}`));
  if (target.writeAllowed !== true) {
    b.blockers.push(issue('TARGET_NOT_WRITABLE', 'danger', `connection '${target.name}' is not marked write-allowed`, { recommendation: 'Mark the target as write-allowed in the copy wizard or with `bulk-cli conn edit --write-allowed true`' }));
  }
  if (source.id === target.id && req.source.database === req.target.database) {
    b.blockers.push(issue('SAME_DATABASE', 'danger', 'the target is the source database'));
  }
  if (inp.busyTargets(target.id, req.target.database, inp.resume?.runId)) {
    b.blockers.push(issue('TARGET_IN_USE', 'danger', `another copy job writes into ${target.name}.${req.target.database}`));
  }
  if (target.tag === 'production') {
    if (!inp.atStart) {
      b.warnings.push(issue('PRODUCTION_CONFIRM', 'info', `target '${target.name}' is tagged production: its name must be typed to start`));
    } else if (o.confirmProduction !== target.name) {
      b.blockers.push(issue('PRODUCTION_CONFIRM', 'danger', `target '${target.name}' is tagged production: confirm_production does not match its name`));
    }
  }

  const c = Math.min(4, Math.max(1, o.tableConcurrency));
  const concurrency = family === 'sqlite' ? 1 : c;
  // Checksum verification opens one more session on each side (spec 4.1 "+1 with checksum").
  const verifySessions = o.verify === 'checksum' ? 1 : 0;
  let effective: CopyEffective = {
    loadMethod: family === 'postgres' ? 'pg_copy_csv' : family === 'mysql' ? (tgt.localInfile ? 'mysql_load_data' : 'mysql_insert') : 'sqlite_insert',
    tableConcurrency: concurrency,
    consistentSnapshot: family !== 'mysql' || !src.legacy,
    sourceSessions: concurrency + 1 + verifySessions,
    targetSessions: concurrency + 2 + verifySessions,
  };
  let sessionSql = { source: [] as string[], target: [] as string[] };

  if (family && target.driver === source.driver) {
    const legacyTables = sourceTables(source, family, req.source.database, req.source.schema, src.legacy);
    const generic = legacyTables === null;
    let tables: SrcTable[] =
      legacyTables ??
      GENERIC.map(([name, rows]) => ({ schema: family === 'postgres' ? (req.source.schema ?? 'public') : family === 'sqlite' ? 'main' : req.source.database, name, rows, bytes: rows * ROW_BYTES, kind: 'table' as TableKind }));
    if (req.selection.kind === 'tables') {
      const sel = req.selection.tables;
      tables = tables.filter((t) => t.kind === 'table' || t.kind === 'partitioned' ? sel.some((r) => r.name === t.name && (family !== 'postgres' || !r.schema || r.schema === t.schema)) : true);
    }
    if (!o.objects.views) tables = tables.filter((t) => t.kind !== 'view' && t.kind !== 'materialized_view');
    const tv = targetView(target, family, req.target.database);

    if (family === 'mysql') {
      const eff = mysqlEffective(req, src, tgt);
      effective = { ...effective, charset: eff.charset, collation: eff.collation, engine: eff.engine, zeroDates: eff.zero, chunkBytes: BigInt(64 * 1024 * 1024), ...(o.mysql.timeZone ? { timeZone: o.mysql.timeZone } : {}) };
      planMysql(inp, b, tables, tv, src, tgt, eff, generic);
      sessionSql = {
        source: src.legacy ? ['SET SQL_BIG_SELECTS = 1', 'SET SESSION net_write_timeout = 600'] : ['SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ', 'SET NAMES utf8mb4', "SET SESSION time_zone = '+00:00'"],
        target: [
          `SET NAMES ${eff.charset}`,
          `SET SESSION sql_mode = '${eff.zero === 'keep' ? 'STRICT_ALL_TABLES,ALLOW_INVALID_DATES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO' : 'STRICT_ALL_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'}'`,
          "SET SESSION time_zone = '+00:00'",
          `SET SESSION innodb_lock_wait_timeout = ${o.targetLockTimeoutS ?? 5}`,
          'SET SESSION foreign_key_checks = 0',
        ],
      };
    } else if (family === 'postgres') {
      effective = { ...effective, charset: 'UTF8' };
      planPostgres(inp, b, tables, tv, src, tgt, generic);
      sessionSql = {
        source: [
          'SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY',
          'SET row_security = off',
          `SET work_mem = '${source.session.workMem ?? '64MB'}'`,
          'SET statement_timeout = 0',
        ],
        target: ["SET client_encoding = 'UTF8'", "SET DateStyle = 'ISO, YMD'", `SET lock_timeout = '${o.targetLockTimeoutS ?? 5}s'`, 'SET statement_timeout = 0'],
      };
    } else {
      planSqlite(inp, b, tables, tv, generic);
    }

    // ---- plan-level issues of 3.7
    const tableItems = b.items.filter((i) => i.kind === 'table');
    const skipped = tableItems.filter((i) => i.action === 'skip_existing');
    if (skipped.length > 0) {
      const empty = skipped.filter((i) => i.targetHasRows === false).length;
      for (const it of skipped) {
        it.warnings.push(
          issue('TABLES_SKIPPED_EXISTING', 'warning', `${it.targetName} already exists in the target and is skipped (no DDL, no data)`, {
            itemId: it.id,
            detail: `${skipped.length} tables skipped, ${empty} of them empty`,
            recommendation: 'Empty tables: append or data only',
          }),
        );
      }
      if (o.content === 'data_only' && skipped.length === tableItems.filter((i) => i.action !== 'excluded').length) {
        b.warnings.push(issue('DATA_ONLY_ALL_SKIPPED', 'warning', 'data only: every selected table is skipped', { recommendation: 'Choose truncate or append' }));
      }
    }
    if (!o.objects.sequences && tableItems.some((i) => LOADS.includes(i.action))) {
      b.warnings.push(issue('SEQUENCES_NOT_SET', 'warning', "sequence / AUTO_INCREMENT values are not set: the application's next INSERT on the target may collide with copied keys"));
    }
  }

  const loading = b.items.filter((i) => i.kind === 'table' && LOADS.includes(i.action) && i.hasData);
  const count = (a: PlanAction) => b.items.filter((i) => i.action === a).length;
  const totals = {
    tables: b.items.filter((i) => i.kind === 'table').length,
    objects: b.items.length,
    create: count('create'),
    skip: count('skip_existing'),
    truncate: count('truncate'),
    dropRecreate: count('drop_recreate'),
    append: count('append'),
    unsupported: count('unsupported'),
    excluded: count('excluded'),
    estRows: BigInt(loading.reduce((n, i) => n + Number(i.estRows ?? 0), 0)),
    estBytes: BigInt(loading.reduce((n, i) => n + Number(i.estBytes ?? 0), 0)),
  };

  // Item blockers come from the item warnings marked danger with a blocker code.
  const acks = ackNeeds(b);
  const plan: CopyPlan = {
    planId: '',
    family: family ?? 'sqlite',
    source: factsOf(source, src, req.source.database),
    target: factsOf(target, tgt, req.target.database),
    effective,
    items: b.items,
    warnings: b.warnings,
    blockers: b.blockers,
    acks,
    sessionSql,
    totals,
    ...(inp.resume ? { resumeOf: inp.resume.runId } : {}),
    elapsedMs: BigInt(Math.max(1, Date.now() - started) + 140),
  };
  plan.planId = planIdOf(plan, req);
  return plan;
}

/** Section 2 "Acknowledgement": one need per distinct warning/danger code, keyed by its items. */
function ackNeeds(b: Builder): AckNeed[] {
  const byCode = new Map<string, { severity: Severity; items: Set<string> }>();
  const add = (i: CopyIssue, itemId?: string) => {
    if (i.severity === 'info') return;
    const cur = byCode.get(i.code) ?? { severity: i.severity, items: new Set<string>() };
    if (i.severity === 'danger') cur.severity = 'danger';
    const id = itemId ?? i.itemId;
    if (id) cur.items.add(id);
    byCode.set(i.code, cur);
  };
  for (const w of b.warnings) add(w);
  for (const it of b.items) for (const w of it.warnings) add(w, it.id);
  return [...byCode.entries()]
    .sort(([a], [z]) => a.localeCompare(z))
    .map(([code, v]) => {
      const items = [...v.items].sort();
      return { code, key: items.length ? `${code}#${fnv(items.join('\n'), 2).slice(0, 12)}` : code, severity: v.severity, items };
    });
}

function planIdOf(plan: CopyPlan, req: CopyRequest): string {
  const codes = [...plan.warnings, ...plan.blockers, ...plan.items.flatMap((i) => i.warnings)].map((i) => i.code).filter((c) => !ENV_CODES.has(c));
  const canonical = JSON.stringify({
    family: plan.family,
    s: [plan.source.endpoint, req.source.connId, req.source.database, req.source.schema ?? null],
    t: [plan.target.endpoint, req.target.connId, req.target.database, req.target.schema ?? null],
    e: { ...plan.effective, chunkBytes: plan.effective.chunkBytes === undefined ? null : String(plan.effective.chunkBytes) },
    items: plan.items.map((i) => [i.id, i.kind, i.action, i.phase, i.order, i.targetSql]),
    codes: [...codes].sort(),
  });
  return fnv(canonical, 4);
}

function addTableIssues(b: Builder, it: PlanItem, d: TableDecision): void {
  it.warnings.push(...d.issues);
  if (d.blocker) b.blockers.push(d.blocker);
}

function planPostgres(inp: PlanInput, b: Builder, tables: SrcTable[], tv: TargetView, src: Server, tgt: Server, generic: boolean): void {
  const { req } = inp;
  const o = req.options;
  const cid = catalogId(inp.source);
  const tSchema = (s: string) => (req.source.schema && req.target.schema ? req.target.schema : s);
  const qn = (s: string, n: string) => `${dq(tSchema(s))}.${dq(n)}`;

  // Schemas the copy needs: created when missing (the two-table fallback plans tables only).
  for (const s of generic ? [] : [...new Set(tables.map((t) => t.schema))]) {
    const target = tSchema(s);
    const exists = tv.schemas.has(target);
    newItem(b, { id: `schema:${dq(s)}`, kind: 'schema', name: s, targetName: target, action: exists ? 'skip_existing' : 'create', phase: 'pre_data', existsInTarget: exists, targetSql: exists ? [] : [`CREATE SCHEMA ${dq(target)}`] });
  }

  const tableIds = new Map<string, PlanItem>();
  const longSnapshot: string[] = [];
  for (const t of tables) {
    const id = `table:${dq(t.schema)}.${dq(t.name)}`;
    const label = `${t.schema}.${t.name}`;
    if (t.kind === 'view' || t.kind === 'materialized_view') continue;
    const exists = tv.tables.get(`${tSchema(t.schema)}.${t.name}`);
    const d = decideTable(inp, id, exists, label);
    const cols = generic ? [] : mockColumns(cid, { schema: t.schema, name: t.name });
    const colList = (cols.length ? cols.map((c) => c.name) : ['id', 'name']).map(dq).join(', ');
    const create = generic
      ? `CREATE TABLE ${qn(t.schema, t.name)} (\n    "id" integer NOT NULL,\n    "name" text\n)`
      : `CREATE TABLE ${qn(t.schema, t.name)} (\n${cols.map((c) => `    ${dq(c.name)} ${pgType(c)}${c.nullable ? '' : ' NOT NULL'}${pgDefault(c)}`).join(',\n')}\n)${t.kind === 'partitioned' ? ' PARTITION BY RANGE ("occurred_at")' : ''}`;
    const copy = `COPY ${qn(t.schema, t.name)} (${colList}) FROM STDIN (FORMAT csv)`;
    const hasData = o.content !== 'schema_only';
    const sql: string[] = [];
    if (d.action === 'create' && o.content !== 'data_only') sql.push(create);
    if (d.action === 'truncate') sql.push(`TRUNCATE ONLY ${qn(t.schema, t.name)}`);
    if (d.action === 'drop_recreate') sql.push(`DROP TABLE ${qn(t.schema, t.name)} RESTRICT`, create);
    if (LOADS.includes(d.action) && hasData) sql.push(copy);
    const it = newItem(b, {
      id,
      kind: 'table',
      schema: t.schema,
      targetSchema: tSchema(t.schema),
      name: t.name,
      targetName: t.name,
      action: d.action,
      phase: 'pre_data',
      existsInTarget: d.exists,
      ...(d.exists && d.hasRows !== undefined ? { targetHasRows: d.hasRows } : {}),
      ...(d.exists && d.targetRows !== undefined ? { targetEstRows: BigInt(d.targetRows) } : {}),
      hasData,
      estRows: BigInt(t.rows),
      estBytes: BigInt(t.bytes),
      sourceDdl: generic ? create.replace(qn(t.schema, t.name), `${dq(t.schema)}.${dq(t.name)}`) : mockDdl(cid, { schema: t.schema, name: t.name }),
      targetSql: sql,
      dependsOn: generic ? [] : [`schema:${dq(t.schema)}`],
    });
    addTableIssues(b, it, d);
    tableIds.set(`${t.schema}.${t.name}`, it);
    if (t.bytes > 10 * 1024 ** 3 && LOADS.includes(d.action)) longSnapshot.push(label);
  }
  if (longSnapshot.length > 0) {
    b.warnings.push(issue('SRC_LONG_SNAPSHOT', 'info', `${longSnapshot.length} tables over 10 GB are read under one snapshot: vacuum on the source is held back`, { detail: longSnapshot.join(', ') }));
  }
  if (generic) return;

  // Views and materialized views.
  for (const t of tables.filter((x) => x.kind === 'view' || x.kind === 'materialized_view')) {
    const mat = t.kind === 'materialized_view';
    const id = `${mat ? 'materialized_view' : 'view'}:${dq(t.schema)}.${dq(t.name)}`;
    const exists = tv.tables.has(`${tSchema(t.schema)}.${t.name}`);
    const action: PlanAction = o.exclude.includes(id) ? 'excluded' : exists ? (o.existing === 'drop_recreate' ? 'drop_recreate' : 'skip_existing') : 'create';
    const def = `CREATE ${mat ? 'MATERIALIZED VIEW' : 'VIEW'} ${qn(t.schema, t.name)} AS\nSELECT … FROM ${dq(tSchema(t.schema))}.${dq('orders')}${mat ? '\nWITH NO DATA' : ''}`;
    newItem(b, {
      id,
      kind: mat ? 'materialized_view' : 'view',
      schema: t.schema,
      targetSchema: tSchema(t.schema),
      name: t.name,
      targetName: t.name,
      action,
      phase: mat ? 'post_data' : 'pre_data',
      existsInTarget: exists,
      estRows: BigInt(t.rows),
      sourceDdl: def,
      targetSql: action === 'create' ? [def] : action === 'drop_recreate' ? [`DROP ${mat ? 'MATERIALIZED VIEW' : 'VIEW'} ${qn(t.schema, t.name)} RESTRICT`, def] : [],
    });
    if (mat && o.objects.refreshMatviews && action !== 'skip_existing' && action !== 'excluded') {
      newItem(b, { id: `refresh:${dq(t.schema)}.${dq(t.name)}`, kind: 'refresh', parent: id, schema: t.schema, name: t.name, targetName: t.name, action: 'run', phase: 'finalize', targetSql: [`REFRESH MATERIALIZED VIEW ${qn(t.schema, t.name)}`], dependsOn: [id] });
    }
  }

  // Keys and indexes after the data, then the foreign keys, then the finalize items.
  const postPhase: CopyPhase = o.indexesAfterData ? 'post_data' : 'pre_data';
  const fks: { it: PlanItem; t: SrcTable; col: ColumnInfo; parent?: PlanItem }[] = [];
  for (const t of tables) {
    const table = tableIds.get(`${t.schema}.${t.name}`);
    if (!table) continue;
    const ref = { schema: t.schema, name: t.name };
    for (const ix of mockIndexes(cid, ref)) {
      const kind = ix.primary ? 'constraint' : 'index';
      const sql = ix.primary
        ? `ALTER TABLE ${qn(t.schema, t.name)} ADD CONSTRAINT ${dq(ix.name)} PRIMARY KEY (${ix.columns.map(dq).join(', ')})`
        : (ix.definition ?? '').replace(`${t.schema}.${t.name}`, qn(t.schema, t.name));
      const action = childAction(table.action, table.existsInTarget, false);
      newItem(b, { id: `${kind}:${dq(t.schema)}.${dq(t.name)}.${dq(ix.name)}`, kind, parent: table.id, schema: t.schema, targetSchema: tSchema(t.schema), name: ix.name, targetName: ix.name, action, phase: postPhase, existsInTarget: table.existsInTarget, targetSql: action === 'create' ? [sql] : [], dependsOn: [table.id] });
    }
    for (const col of mockColumns(cid, ref).filter((x) => x.foreignKey)) fks.push({ it: table, t, col, parent: tableIds.get(col.foreignKey!.replace(/\(.*$/, '')) });
  }
  for (const { it, t, col, parent } of fks) {
    const name = `${t.name}_${col.name}_fkey`;
    const id = `foreign_key:${dq(t.schema)}.${dq(t.name)}.${dq(name)}`;
    const target = col.foreignKey!.replace(/\(.*$/, '');
    const [ps, pn] = target.includes('.') ? target.split('.') : [t.schema, target];
    let action = childAction(it.action, it.existsInTarget, false);
    const warnings: CopyIssue[] = [];
    if (!parent) {
      if (action === 'create') action = 'excluded';
      warnings.push(issue('DEPENDENCY_NOT_SELECTED', 'info', `references ${target}, which is not selected`, { itemId: id }));
    } else if (action === 'create' && parent.action === 'skip_existing') {
      warnings.push(issue('FK_TO_SKIPPED', 'warning', `new foreign key ${name} references ${target}, which is skipped: validation may fail`, { itemId: id }));
    }
    newItem(b, {
      id,
      kind: 'foreign_key',
      parent: it.id,
      schema: t.schema,
      targetSchema: tSchema(t.schema),
      name,
      targetName: name,
      action,
      phase: 'post_data',
      existsInTarget: it.existsInTarget,
      targetSql: action === 'create' ? [`ALTER TABLE ${qn(t.schema, t.name)} ADD CONSTRAINT ${dq(name)} FOREIGN KEY (${dq(col.name)}) REFERENCES ${qn(ps, pn)} ("id")`] : [],
      warnings,
      dependsOn: [it.id, ...(parent ? [parent.id] : [])],
    });
  }
  for (const t of tables) {
    const table = tableIds.get(`${t.schema}.${t.name}`);
    if (!table || t.kind !== 'table') continue;
    const seq = `${t.name}_id_seq`;
    if (o.objects.sequences && mockColumns(cid, { schema: t.schema, name: t.name }).some((c) => c.name === 'id')) {
      const action = childAction(table.action, table.existsInTarget, true);
      newItem(b, {
        id: `sequence_value:${dq(t.schema)}.${dq(seq)}`,
        kind: 'sequence_value',
        parent: table.id,
        schema: t.schema,
        targetSchema: tSchema(t.schema),
        name: seq,
        targetName: seq,
        action,
        phase: 'finalize',
        targetSql: action === 'run' ? [`SELECT pg_catalog.setval('${dq(tSchema(t.schema))}.${dq(seq)}', greatest(<source value>, <target value>), true)`] : [],
        dependsOn: [table.id],
      });
    }
    if (o.objects.analyze && LOADS.includes(table.action) && table.hasData) {
      newItem(b, { id: `analyze:${dq(t.schema)}.${dq(t.name)}`, kind: 'analyze', parent: table.id, schema: t.schema, targetSchema: tSchema(t.schema), name: t.name, targetName: t.name, action: 'run', phase: 'finalize', targetSql: [`ANALYZE ${qn(t.schema, t.name)}`], dependsOn: [table.id] });
    }
  }
  if (tgt.major !== src.major && /17/.test(inp.target.host ?? '')) {
    b.warnings.push(issue('PG_DEFAULT_COLLATION_DIFFERS', 'warning', `the target database uses the builtin locale provider (C.UTF-8); the source uses libc en_US.utf8`, { recommendation: 'Check ORDER BY and LIKE behaviour of the application' }));
  }
}

function planMysql(inp: PlanInput, b: Builder, tables: SrcTable[], tv: TargetView, src: Server, tgt: Server, eff: EffectiveMysql, generic: boolean): void {
  const { req } = inp;
  const o = req.options;
  const cid = catalogId(inp.source);
  const db = req.target.database;
  const qn = (n: string) => `${bq(db)}.${bq(n)}`;
  if (!tv.dbExists) {
    newItem(b, { id: `database:${dq(db)}`, kind: 'database', name: req.source.database, targetName: db, action: 'create', phase: 'pre_data', targetSql: [`CREATE DATABASE ${bq(db)} DEFAULT CHARACTER SET ${eff.charset} COLLATE ${eff.collation}`] });
  }
  if (src.legacy) {
    b.warnings.push(
      issue('SRC_NOT_SNAPSHOT', 'warning', 'MyISAM source tables cannot be read under one consistent snapshot: rows written meanwhile may be missing or inconsistent between tables', {
        recommendation: 'Stop the application writes during the copy',
      }),
    );
    b.warnings.push(issue('TIMESTAMP_SEMANTICS', 'info', 'MySQL 4.0 TIMESTAMP(n) display widths are dropped; the first TIMESTAMP column keeps its automatic initialisation and update'));
    b.warnings.push(issue('COLLATION_CHANGED', 'info', `text columns move from euckr_korean_ci to ${eff.collation}: ORDER BY and comparisons follow the new collation`));
    if (eff.engine === 'InnoDB') b.warnings.push(issue('ENGINE_CHANGED', 'info', 'MyISAM tables are created as InnoDB'));
  }
  if (!tgt.localInfile) b.warnings.push(issue('LOCAL_INFILE_OFF', 'info', `local_infile is OFF on ${tgt.version}: rows are sent as multi-row INSERT batches under max_allowed_packet`));
  if (!atLeast(tgt, 8)) b.warnings.push(issue('AUTOINC_NOT_PERSISTED', 'info', 'servers before 8.0 reset AUTO_INCREMENT counters to max+1 on restart'));

  const items = new Map<string, PlanItem>();
  for (const t of tables) {
    const id = `table:${dq(t.name)}`;
    const exists = tv.tables.get(t.name);
    const d = decideTable(inp, id, exists, t.name);
    const override = o.mysql.tableOverrides.find((x) => x.table === t.name);
    const collation = override?.collation ?? eff.collation;
    const charset = override?.charset ?? eff.charset;
    const ref = { schema: t.schema, name: t.name };
    const srcCols = generic ? [] : mockColumns(cid, ref);
    const cols: MysqlColumn[] = generic
      ? [
          { name: 'id', src: 'int(11) NOT NULL auto_increment', tgt: `${atLeast(tgt, 8, 0, 17) ? 'int' : 'int(11)'} NOT NULL AUTO_INCREMENT` },
          { name: 'name', src: "varchar(40) NOT NULL default ''", tgt: "varchar(40) NOT NULL DEFAULT ''" },
        ]
      : mysqlColumns(srcCols, tgt, eff.zero, src.legacy);
    // Unique keys come from the columns; the other keys are created after the data.
    const indexes = generic ? [] : mockIndexes(cid, ref).filter((i) => !i.primary && !i.unique);
    const uniques = srcCols.filter((c) => c.unique && !c.primaryKey);
    const pk = generic ? ['id'] : srcCols.filter((c) => c.primaryKey).map((c) => c.name);
    // What the source server prints for SHOW CREATE TABLE (4.0: `TYPE=MyISAM`).
    const srcDdl = generic
      ? [`CREATE TABLE ${bq(t.name)} (`, [...cols.map((c) => `  ${bq(c.name)} ${c.src}`), '  PRIMARY KEY  (`id`)'].join(',\n'), `) TYPE=${src.legacy ? 'MyISAM' : 'InnoDB'}`].join('\n')
      : mockDdl(cid, ref);
    const create = [
      `CREATE TABLE ${qn(t.name)} (`,
      [...cols.map((c) => `  ${bq(c.name)} ${c.tgt}`), ...(pk.length ? [`  PRIMARY KEY (${pk.map(bq).join(', ')})`] : [])].join(',\n'),
      `) ENGINE=${eff.engine} DEFAULT CHARSET=${charset} COLLATE=${collation}`,
    ].join('\n');
    const colList = cols.map((c) => bq(c.name)).join(', ');
    const load =
      tgt.localInfile
        ? `LOAD DATA LOCAL INFILE 'bulkdb-<stream>' INTO TABLE ${qn(t.name)} CHARACTER SET ${charset} FIELDS TERMINATED BY '\\t' ESCAPED BY '\\\\' LINES TERMINATED BY '\\n' (${colList})`
        : `INSERT INTO ${qn(t.name)} (${colList}) VALUES <rows>`;
    const hasData = o.content !== 'schema_only';
    const sql: string[] = [];
    if (d.action === 'create' && o.content !== 'data_only') sql.push(create);
    if (d.action === 'truncate') sql.push(`TRUNCATE TABLE ${qn(t.name)}`);
    if (d.action === 'drop_recreate') sql.push(`DROP TABLE ${qn(t.name)}`, create);
    if (LOADS.includes(d.action) && hasData) sql.push(load);
    const it = newItem(b, {
      id,
      kind: 'table',
      name: t.name,
      targetName: t.name,
      action: d.action,
      phase: 'pre_data',
      existsInTarget: d.exists,
      ...(d.exists && d.hasRows !== undefined ? { targetHasRows: d.hasRows } : {}),
      ...(d.exists && d.targetRows !== undefined ? { targetEstRows: BigInt(d.targetRows) } : {}),
      hasData,
      estRows: BigInt(t.rows),
      estBytes: BigInt(t.bytes),
      sourceDdl: srcDdl,
      targetSql: sql,
      dependsOn: tv.dbExists ? [] : [`database:${dq(db)}`],
    });
    addTableIssues(b, it, d);
    items.set(t.name, it);
    const creates = d.action === 'create' || d.action === 'drop_recreate';
    if (creates || LOADS.includes(d.action)) {
      for (const c of cols.filter((x) => x.zeroDate)) {
        const code = eff.zero === 'null' ? 'ZERO_DATE_NULL' : eff.zero === 'min' ? 'ZERO_DATE_MIN' : 'ZERO_DATE_KEEP';
        it.warnings.push(issue(code, 'warning', `${t.name}.${c.name}: zero dates ('0000-00-00 00:00:00') are ${eff.zero === 'null' ? 'loaded as NULL' : eff.zero === 'min' ? "replaced by '1000-01-01 00:00:00'" : 'kept'}`, { itemId: id, detail: c.name }));
      }
      if (cols.some((c) => c.decimal)) it.warnings.push(issue('DECIMAL_WIDENED', 'info', `${t.name}: DECIMAL columns gain one digit (MySQL 4.0 counted the sign in the precision)`, { itemId: id }));
      if (src.legacy && t.bytes > 1024 ** 3) it.warnings.push(issue('SRC_MYISAM_LOCK', 'warning', `${t.name} is a MyISAM table over 1 GB: its writers wait while it is read`, { itemId: id }));
      if (src.legacy && uniques.length > 0 && !collation.endsWith('_bin')) {
        it.warnings.push(
          issue('COLLATION_UNIQUE_RISK', 'warning', `${t.name}: UNIQUE ${uniques.map((u) => u.name).join(', ')} under ${collation} treats values that differ only by width, case or trailing spaces as equal`, {
            itemId: id,
            recommendation: 'Give this table a _bin collation',
          }),
        );
      }
      const dupEnum = cols.find((c) => c.enumCaseDuplicate);
      if (dupEnum && !collation.endsWith('_bin')) {
        b.blockers.push(
          issue('ENUM_DUPLICATE', 'danger', `${t.name}.${dupEnum.name}: ENUM values that differ only by case are duplicates under ${collation}`, {
            itemId: id,
            detail: dupEnum.src,
            recommendation: 'Give this table a _bin collation (utf8mb4_bin)',
          }),
        );
      }
      if (!atLeast(tgt, 5, 7) && uniques.some((u) => /255/.test(u.dataType))) {
        it.warnings.push(issue('UNIQUE_PREFIX', 'danger', `${t.name}: the unique key over a varchar(255) becomes a 191-character prefix on ${tgt.version} (767-byte keys)`, { itemId: id, recommendation: 'A target with DYNAMIC rows (5.7+) keeps the full key' }));
      }
    }
    // Keys and indexes after the data: one item each.
    for (const u of uniques) {
      const action = childAction(it.action, it.existsInTarget, false);
      newItem(b, { id: `index:${dq(t.name)}.${dq(u.name)}`, kind: 'index', parent: id, name: u.name, targetName: u.name, action, phase: 'post_data', existsInTarget: it.existsInTarget, targetSql: action === 'create' ? [`ALTER TABLE ${qn(t.name)} ADD UNIQUE INDEX ${bq(u.name)} (${bq(u.name)})`] : [], dependsOn: [id] });
    }
    for (const ix of indexes) {
      const action = childAction(it.action, it.existsInTarget, false);
      newItem(b, { id: `index:${dq(t.name)}.${dq(ix.name)}`, kind: 'index', parent: id, name: ix.name, targetName: ix.name, action, phase: 'post_data', existsInTarget: it.existsInTarget, targetSql: action === 'create' ? [`ALTER TABLE ${qn(t.name)} ADD INDEX ${bq(ix.name)} (${ix.columns.map(bq).join(', ')})`] : [], dependsOn: [id] });
    }
    if (o.objects.sequences && !generic) {
      const action = childAction(it.action, it.existsInTarget, true);
      newItem(b, { id: `auto_increment:${dq(t.name)}`, kind: 'auto_increment', parent: id, name: t.name, targetName: t.name, action, phase: 'finalize', targetSql: action === 'run' ? [`ALTER TABLE ${qn(t.name)} AUTO_INCREMENT = <source value>`] : [], dependsOn: [id] });
    }
    if (o.objects.analyze && LOADS.includes(it.action) && hasData && !generic) {
      newItem(b, { id: `analyze:${dq(t.name)}`, kind: 'analyze', parent: id, name: t.name, targetName: t.name, action: 'run', phase: 'finalize', targetSql: [`ANALYZE TABLE ${qn(t.name)}`], dependsOn: [id] });
    }
  }
}

function planSqlite(inp: PlanInput, b: Builder, tables: SrcTable[], tv: TargetView, generic: boolean): void {
  const o = inp.req.options;
  for (const t of tables) {
    if (t.kind === 'view' || t.kind === 'materialized_view') continue;
    const id = `table:${dq(t.name)}`;
    const d = decideTable(inp, id, tv.tables.get(t.name), t.name);
    const cols = generic ? [] : mockColumns(LOCAL_CONN_ID, { schema: 'main', name: t.name });
    const names = cols.length ? cols.map((c) => c.name) : ['id', 'name'];
    const create = generic
      ? `CREATE TABLE ${dq(t.name)} (\n    "id" INTEGER PRIMARY KEY,\n    "name" TEXT\n)`
      : `CREATE TABLE ${dq(t.name)} (\n${cols.map((c) => `    ${dq(c.name)} ${c.dataType}${c.primaryKey ? ' PRIMARY KEY' : c.nullable ? '' : ' NOT NULL'}${c.default ? ` DEFAULT ${c.default}` : ''}`).join(',\n')}\n)`;
    const insert = `INSERT INTO ${dq(t.name)} (${names.map(dq).join(', ')}) VALUES (${names.map((_, i) => `?${i + 1}`).join(', ')})`;
    const hasData = o.content !== 'schema_only';
    const sql: string[] = [];
    if (d.action === 'create' && o.content !== 'data_only') sql.push(create);
    if (d.action === 'truncate') sql.push(`DELETE FROM ${dq(t.name)}`);
    if (d.action === 'drop_recreate') sql.push(`DROP TABLE ${dq(t.name)}`, create);
    if (LOADS.includes(d.action) && hasData) sql.push(insert);
    const it = newItem(b, {
      id,
      kind: 'table',
      name: t.name,
      targetName: t.name,
      action: d.action,
      phase: 'pre_data',
      existsInTarget: d.exists,
      ...(d.exists && d.hasRows !== undefined ? { targetHasRows: d.hasRows } : {}),
      hasData,
      estRows: BigInt(t.rows),
      estBytes: BigInt(t.bytes),
      sourceDdl: create,
      targetSql: sql,
    });
    addTableIssues(b, it, d);
    if (generic) continue;
    for (const ix of mockIndexes(LOCAL_CONN_ID, { schema: 'main', name: t.name }).filter((i) => !i.primary)) {
      const action = childAction(it.action, it.existsInTarget, false);
      newItem(b, { id: `index:${dq(t.name)}.${dq(ix.name)}`, kind: 'index', parent: id, name: ix.name, targetName: ix.name, action, phase: 'post_data', existsInTarget: it.existsInTarget, targetSql: action === 'create' && ix.definition ? [ix.definition] : [], dependsOn: [id] });
    }
    if (o.objects.analyze && LOADS.includes(it.action) && hasData) {
      newItem(b, { id: `analyze:${dq(t.name)}`, kind: 'analyze', parent: id, name: t.name, targetName: t.name, action: 'run', phase: 'finalize', targetSql: ['ANALYZE'], dependsOn: [id] });
    }
  }
}

// ---- the simulated run -----------------------------------------------------------------------

interface SimTable {
  itemId: string;
  name: string;
  est: number;
  rows: number;
  /** Data ticks this table needs. */
  work: number;
  done: number;
  state: CopyItemState;
  seq: number;
  startedAt?: number;
  finishedAt?: number;
  loads: boolean;
  /** This job runs CREATE TABLE for it (create / drop_recreate). */
  creates: boolean;
  createdByRun: boolean;
  method: LoadMethod;
  fixes: ValueFix[];
  unmappable: number;
  error?: { code: string; message: string; detail?: string };
}

/** What the fake source holds that conversion changes (legacy MySQL demo; section 3.4 / 5.6). */
function valueFixesOf(plan: CopyPlan, item: PlanItem, rows: number): { fixes: ValueFix[]; unmappable: number } {
  const fixes: ValueFix[] = [];
  let unmappable = 0;
  if (plan.family !== 'mysql' || !plan.source.version.startsWith('4.')) return { fixes, unmappable };
  for (const w of item.warnings) {
    if (w.code.startsWith('ZERO_DATE_') && w.detail) fixes.push({ column: w.detail, code: w.code, count: BigInt(Math.max(1, Math.round(rows * 0.0021))) });
  }
  if (item.name === 'orders') fixes.push({ column: 'shipped_at', code: 'INVALID_DATE', count: BigInt(3) });
  if (item.name === 'customers') unmappable = 3;
  return { fixes, unmappable };
}

export function createCopyHandlers(ctx: MockCtx): MockFeature {
  const runs = new Map<string, MockRun>();
  /** job id → run id */
  const jobRuns = new Map<string, string>();
  const statuses = new Map<string, CopyStatus>();
  const planWaits = new Map<string, (cancelled: boolean) => void>();

  const jobState = (jobId: string): JobState | undefined => ctx.jobs.get(jobId)?.state ?? statuses.get(jobId)?.state;
  const inUse = (run: MockRun): boolean => {
    const s = jobState(run.jobId);
    return !!s && ACTIVE.includes(s);
  };
  const busyTargets = (connId: string, db: string, exceptRun?: string): boolean =>
    [...runs.values()].some((r) => r.runId !== exceptRun && !r.dryRun && r.req.target.connId === connId && r.req.target.database === db && inUse(r));

  const info = (run: MockRun): CopyRunInfo => {
    const state = jobState(run.jobId) ?? 'pending';
    const p = statuses.get(run.jobId)?.progress;
    const failed = p?.tables.filter((t) => t.state === 'failed').length ?? 0;
    return {
      runId: run.runId,
      label: run.label,
      sourceConnId: run.req.source.connId,
      targetConnId: run.req.target.connId,
      sourceDatabase: run.req.source.database,
      targetDatabase: run.req.target.database,
      startedAt: run.startedAt,
      ...(run.finishedAt ? { finishedAt: run.finishedAt } : {}),
      state,
      lastJobId: run.jobId,
      tablesTotal: p?.tablesTotal ?? run.plan.totals.tables,
      tablesDone: p?.tablesDone ?? 0,
      tablesFailed: failed,
      resumable: !run.dryRun && !inUse(run) && (state === 'failed' || state === 'cancelled' || state === 'rejected' || state === 'done'),
      dryRun: run.dryRun,
      inUse: inUse(run),
    };
  };

  /** The request as stored for a run: no passwords, no start-time fields. */
  const storedRequest = (req: CopyRequest): CopyRequest => {
    const { confirmProduction: _c, ...options } = req.options;
    void _c;
    const side = (e: CopyRequest['source']) => ({ connId: e.connId, database: e.database, ...(e.schema ? { schema: e.schema } : {}) });
    return { ...req, source: side(req.source), target: side(req.target), options: { ...options, dryRun: false } };
  };

  const plan = (req: CopyRequest, atStart: boolean, resume?: ResumeState): CopyPlan =>
    buildPlan({ req, source: ctx.findConn(req.source.connId), target: ctx.findConn(req.target.connId), resume, atStart, busyTargets });

  /** Plans by id, newest last (core's recent plans): the base of `PLAN_CHANGED` details. */
  const recentPlans = new Map<string, CopyPlan>();
  const remember = (p: CopyPlan) => {
    recentPlans.delete(p.planId);
    recentPlans.set(p.planId, p);
    while (recentPlans.size > RECENT_PLANS) recentPlans.delete(recentPlans.keys().next().value!);
  };

  /** Core `preflight::plan_diff`: items whose action or statements changed, new or gone. */
  const planDiff = (old: CopyPlan, now: CopyPlan): string[] => {
    const before = new Map(old.items.map((i) => [i.id, i]));
    const out = now.items
      .filter((i) => {
        const o = before.get(i.id);
        return !o || o.action !== i.action || JSON.stringify(o.targetSql) !== JSON.stringify(i.targetSql);
      })
      .map((i) => i.id);
    const ids = new Set(now.items.map((i) => i.id));
    return [...out, ...old.items.filter((i) => !ids.has(i.id)).map((i) => i.id)];
  };

  /** The plan differs from the reviewed one: a `PLAN_CHANGED` warning and its acknowledgement (core 5.1). */
  const markPlanChanged = (p: CopyPlan, reviewed: string): void => {
    const old = recentPlans.get(reviewed);
    const ids = old ? planDiff(old, p) : [];
    const detail = !old
      ? `the reviewed plan ${reviewed} is not known to this process; review the new plan`
      : ids.length > 0
        ? `changed items: ${ids.join(', ')}`
        : 'the issues or the effective settings changed';
    p.warnings.push(issue('PLAN_CHANGED', 'warning', 'the plan changed since it was reviewed', { detail }));
    p.acks.push({ code: 'PLAN_CHANGED', key: 'PLAN_CHANGED', severity: 'warning', items: [] });
  };

  /** The report a warned copy job waits on: one warning per missing key (core 5.1 step 3). */
  const warnedReport = (p: CopyPlan, missing: AckNeed[]): PreflightReport => {
    const all = [...p.warnings, ...p.items.flatMap((i) => i.warnings)];
    return {
      warnings: missing.map((a) => ({
        code: a.key,
        severity: a.severity,
        message: all.find((w) => w.code === a.code)?.message ?? a.code,
        ...(a.items.length ? { detail: a.items.join('\n') } : {}),
      })),
      blockers: [],
      estimates: { outputPath: '' },
      sessions: [],
      elapsedMs: p.elapsedMs,
    };
  };

  /**
   * Run (or resume) the fake job of `run` with `p`. With `missing` acknowledgements the job waits
   * in `warned` after its preflight until `job_resume` names every missing key; with blockers in
   * `p` it ends `rejected` after its preflight.
   */
  const launch = (run: MockRun, p: CopyPlan, dryRun: boolean, missing: AckNeed[] = []): string => {
    const source = ctx.findConn(run.req.source.connId);
    const target = ctx.findConn(run.req.target.connId);
    const tableItems = p.items.filter((i) => i.kind === 'table');
    const copy: CopyJobInfo = {
      runId: run.runId,
      targetConnId: target.id,
      targetConnName: target.name,
      sourceDatabase: run.req.source.database,
      targetDatabase: run.req.target.database,
      phase: 'plan',
      // Known once planned (core `CopyJob::set_plan`).
      tablesTotal: 0,
      tablesDone: 0,
      tablesFailed: 0,
      dryRun,
    };
    const jobId = ctx.jobs.create('copy', source.id, run.label, copy);
    if (!run.runId) {
      run.runId = jobId;
      copy.runId = jobId;
    }
    const summaryCopy = () => ctx.jobs.get(jobId)?.copy;
    const sc = summaryCopy();
    if (sc) sc.runId = run.runId;
    run.jobId = jobId;
    run.plan = p;
    run.finishedAt = undefined;
    jobRuns.set(jobId, run.runId);
    runs.set(run.runId, run);

    const conc = p.effective.tableConcurrency;
    const loads = tableItems.filter((i) => LOADS.includes(i.action));
    const weight = (i: PlanItem) => Math.log10(Number(i.estRows ?? 0) + 10);
    const total = loads.reduce((n, i) => n + weight(i), 0) || 1;
    let seq = 0;
    const sim: SimTable[] = tableItems
      .filter((i) => i.action !== 'excluded' && i.action !== 'unsupported')
      .map((i) => {
        const prior = run.checkpoints.get(i.id);
        const isLoad = LOADS.includes(i.action) && i.hasData;
        const est = Number(i.estRows ?? 0);
        const done = i.action === 'resume_done';
        return {
          itemId: i.id,
          name: i.targetName,
          est,
          rows: done ? (prior?.rows ?? est) : 0,
          work: isLoad ? Math.max(1, Math.round((DATA_TICKS * weight(i)) / total)) : 0,
          done: 0,
          state: i.action === 'skip_existing' ? 'skipped' : done ? 'done' : 'pending',
          seq: 0,
          loads: isLoad,
          creates: (i.action === 'create' || i.action === 'drop_recreate') && run.req.options.content !== 'data_only',
          createdByRun: !!prior?.createdByRun,
          method: p.effective.loadMethod,
          ...valueFixesOf(p, i, est),
        };
      });
    const touched = new Set<SimTable>();
    const set = (t: SimTable, patch: Partial<SimTable>) => {
      Object.assign(t, patch);
      touched.add(t);
    };
    const entry = (t: SimTable, now: number): CopyTableProgress => ({
      seq: BigInt(t.seq),
      itemId: t.itemId,
      state: t.state,
      rowsRead: BigInt(t.rows),
      rowsWritten: BigInt(t.rows),
      bytes: BigInt(t.rows * ROW_BYTES),
      estRows: BigInt(t.est),
      rateRowsPerSec: t.state === 'loading' && t.startedAt ? Math.round(t.rows / Math.max(0.05, (now - t.startedAt) / 1000)) : 0,
      ...(t.startedAt ? { elapsedMs: BigInt((t.finishedAt ?? now) - t.startedAt) } : {}),
      ...(t.loads ? { method: t.method } : {}),
      ...(t.error ? { error: t.error } : {}),
    });
    let phase: CopyPhase = 'plan';
    const started = Date.now();
    const progress = (full: boolean, now: number): CopyProgress => {
      const list = full ? sim : sim.filter((t) => touched.has(t));
      const doneN = sim.filter((t) => t.state === 'done' || t.state === 'planned').length;
      const skippedN = sim.filter((t) => t.state === 'skipped').length;
      return {
        jobId,
        runId: run.runId,
        seq: BigInt(seq),
        phase,
        itemsTotal: p.items.length,
        itemsDone: doneN,
        itemsFailed: sim.filter((t) => t.state === 'failed').length,
        itemsSkipped: skippedN,
        tablesTotal: sim.length,
        tablesDone: doneN + skippedN,
        tables: list.slice(0, full ? undefined : 64).map((t) => entry(t, now)),
        full,
      };
    };
    const status: CopyStatus = { jobId, runId: run.runId, state: 'pending', progress: { ...progress(true, started), tables: [] } };
    statuses.set(jobId, status);

    /** One step of the simulation: apply, then emit what changed. */
    const publish = (now: number) => {
      if (touched.size === 0 && status.progress.phase === phase) return;
      seq += 1;
      for (const t of touched) t.seq = seq;
      status.progress = progress(true, now);
      const delta = progress(false, now);
      touched.clear();
      ctx.emit('copy:progress', delta);
      const rows = sim.reduce((n, t) => n + (t.state === 'skipped' ? 0 : t.rows), 0);
      const elapsedMs = now - started;
      const rate = rows / Math.max(0.05, elapsedMs / 1000);
      const left = sim.filter((t) => t.loads).reduce((n, t) => n + Math.max(0, t.est - t.rows), 0);
      const jp: JobProgress = {
        jobId,
        rows: BigInt(rows),
        bytesIn: BigInt(rows * ROW_BYTES),
        bytesOut: BigInt(rows * ROW_BYTES),
        batches: BigInt(seq),
        files: 0,
        elapsedMs: BigInt(elapsedMs),
        rateRowsPerSec: Math.round(rate),
        rateBytesPerSec: Math.round(rate * ROW_BYTES),
        ...(rate > 0 && left > 0 ? { etaSec: BigInt(Math.ceil(left / rate)) } : {}),
        phase: phase === 'data' ? 'transferring' : phase === 'pre_data' || phase === 'plan' ? 'preparing' : 'finishing',
      };
      ctx.jobs.setProgress(jobId, jp);
      const c = summaryCopy();
      if (c) {
        c.phase = phase;
        c.tablesDone = status.progress.tablesDone;
        c.tablesFailed = status.progress.itemsFailed;
      }
    };

    const checkpoint = () => {
      for (const t of sim) {
        const state: CheckpointState =
          t.state === 'done' ? 'done' : t.state === 'skipped' ? 'skipped' : t.state === 'failed' ? 'failed' : t.state === 'pending' ? (t.createdByRun ? 'created' : 'pending') : 'loading';
        const prior = run.checkpoints.get(t.itemId);
        run.checkpoints.set(t.itemId, { state, createdByRun: t.createdByRun || !!prior?.createdByRun, rows: t.rows });
      }
    };

    const result = (now: number, cancelled: boolean): CopyResult => {
      const verify = run.req.options.verify;
      const tables: CopyTableResult[] = sim.map((t) => {
        const problems: string[] = [];
        if (t.unmappable > 0 && run.req.options.onUnmappable === 'replace') problems.push('UNMAPPABLE_CHARS');
        if (t.fixes.some((f) => f.code === 'INVALID_DATE')) problems.push('INVALID_DATE');
        const loaded = t.state === 'done' && t.loads && !dryRun;
        // Planned zero-date conversions are the chosen policy; replacements nobody planned
        // (unmappable characters, invalid calendar dates) make the values differ (5.6).
        const lossy = loaded && (t.unmappable > 0 || t.fixes.some((f) => f.code === 'INVALID_DATE'));
        return {
          itemId: t.itemId,
          targetName: t.name,
          state: t.state,
          rowsRead: BigInt(t.rows),
          rowsWritten: BigInt(t.rows),
          ...(loaded && verify !== 'none' ? { targetCount: BigInt(t.rows) } : {}),
          verify: !loaded || verify === 'none' ? 'not_run' : lossy ? 'lossy' : 'match',
          ...(loaded && verify === 'checksum' ? { checksumSource: fnv(t.itemId, 2), checksumTarget: fnv(t.itemId, 2) } : {}),
          unmappableChars: BigInt(loaded ? t.unmappable : 0),
          dataWarnings: BigInt(0),
          warningSamples: [],
          valueFixes: loaded ? t.fixes : [],
          problems: loaded ? problems : [],
          durationMs: BigInt(t.startedAt ? (t.finishedAt ?? now) - t.startedAt : 0),
          ...(t.loads ? { method: t.method } : {}),
          ...(t.error ? { error: t.error } : {}),
        };
      });
      const objects: CopyItemResult[] = p.items
        .filter((i) => i.kind !== 'table' && i.action !== 'excluded')
        .map((i) => ({ itemId: i.id, state: dryRun ? 'planned' : cancelled ? (phase === 'finalize' || phase === 'verify' ? 'done' : 'pending') : i.action === 'skip_existing' ? 'skipped' : 'done' }));
      const leftBehind: LeftBehind[] = [];
      if (cancelled) {
        for (const t of sim) {
          if (t.state === 'cancelled' || t.state === 'pending') {
            if (t.createdByRun) leftBehind.push({ itemId: t.itemId, kind: 'created_table' });
            if (t.rows > 0 && p.family === 'mysql') leftBehind.push({ itemId: t.itemId, kind: 'partial_rows', rows: BigInt(t.rows) });
            const item = p.items.find((i) => i.id === t.itemId);
            if (item?.action === 'truncate' && phase !== 'plan') leftBehind.push({ itemId: t.itemId, kind: 'emptied_table' });
          }
        }
        const post = p.items.find((i) => i.phase === 'post_data' && i.action === 'create');
        if (post && phase !== 'finalize' && phase !== 'verify') leftBehind.push({ itemId: post.parent ?? post.id, kind: 'missing_post_data' });
        const seqItem = p.items.find((i) => (i.kind === 'sequence_value' || i.kind === 'auto_increment') && i.action === 'run');
        if (seqItem) leftBehind.push({ itemId: seqItem.id, kind: 'sequence_not_set' });
      }
      const warnings: CopyIssue[] = [];
      const unmappable = tables.reduce((n, t) => n + Number(t.unmappableChars), 0);
      if (unmappable > 0) warnings.push(issue('UNMAPPABLE_CHARS', 'warning', `${unmappable} characters could not be converted to ${p.effective.charset ?? 'UTF-8'} and were replaced`));
      if (cancelled && p.family === 'mysql' && sim.some((t) => t.state === 'cancelled' && t.rows > 0)) {
        warnings.push(issue('SERVER_ROLLBACK_PENDING', 'info', 'the target may still be rolling back the last open chunk'));
      }
      const problems = tables.filter((t) => t.problems.length > 0).length;
      const loadedTables = tables.filter((t) => t.state === 'done' && sim.find((s) => s.itemId === t.itemId)?.loads);
      return {
        runId: run.runId,
        dryRun,
        existing: run.req.options.existing,
        tables,
        objects,
        warnings,
        leftBehind,
        verified: !dryRun && !cancelled && verify !== 'none' && problems === 0 && loadedTables.every((t) => t.verify === 'match'),
        problems,
        skippedExisting: sim.filter((t) => t.state === 'skipped').length,
        resumable: !dryRun,
        durationMs: BigInt(now - started),
      };
    };

    let timer: ReturnType<MockCtx['every']> | undefined;
    ctx.jobs.onCancel(jobId, () => {
      ctx.clear(timer);
      const now = Date.now();
      for (const t of sim) {
        if (t.state === 'creating' || t.state === 'loading' || t.state === 'indexing' || t.state === 'verifying') set(t, { state: 'cancelled', finishedAt: now });
      }
      publish(now);
      checkpoint();
      status.state = 'cancelled';
      status.result = result(now, true);
      run.result = status.result;
      run.finishedAt = ctx.nowIso();
      ctx.log('warn', `Copy ${run.label}: cancelled; ${status.result.leftBehind.length} things stay in the target`, jobId, 'bulkdb::copy');
    });

    const tick = ctx.options().progressTickMs * COPY_TICK_FACTOR;
    const preTicks = dryRun ? 1 : 2;
    let step = 0;
    ctx.after(tick, () => {
      // Cancelled while pending: the job already ended.
      if (status.state === 'cancelled') return;
      ctx.jobs.setState(jobId, 'preflight');
      status.state = 'preflight';
      status.plan = p;
      // Planned: every table is listed (pending, skipped or done earlier in the run) with the
      // table count, and the shell's next poll sends that delta to every window.
      const c = summaryCopy();
      if (c) c.tablesTotal = sim.length;
      for (const t of sim) touched.add(t);
      publish(Date.now());
      status.progress = progress(true, Date.now());
      ctx.log('info', `Copy plan ${p.planId}: ${p.totals.tables} tables, ${p.totals.objects} items`, jobId, 'bulkdb::copy');
      const goRunning = () => {
        ctx.jobs.setState(jobId, 'running');
        status.state = 'running';
        timer = ctx.every(tick, () => {
          const now = Date.now();
          step += 1;
          if (step <= preTicks) {
            // Structure first (a dry run executes DDL only on PostgreSQL, and rolls it back).
            phase = 'pre_data';
            if (step === 1 && (!dryRun || p.family === 'postgres')) {
              for (const t of sim) if (t.state === 'pending' && t.creates) set(t, { state: 'creating', startedAt: now });
            }
            if (step === preTicks) {
              for (const t of sim) if (t.state === 'creating') set(t, { state: 'pending', startedAt: undefined, createdByRun: !dryRun });
              if (dryRun) {
                for (const t of sim) if (t.state === 'pending') set(t, { state: 'planned', finishedAt: now });
                phase = 'verify';
              }
            }
            publish(now);
            if (!dryRun || step < preTicks) return;
          } else if (!dryRun) {
            const active = sim.filter((t) => t.loads && (t.state === 'pending' || t.state === 'loading'));
            if (active.length > 0) {
              phase = 'data';
              for (const t of sim) if (t.state === 'indexing') set(t, { state: 'done', finishedAt: now });
              for (const t of active.slice(0, conc)) {
                const done = t.done + 1;
                const finished = done >= t.work;
                const failed = finished && t.unmappable > 0 && run.req.options.onUnmappable === 'fail';
                set(t, {
                  done,
                  startedAt: t.state === 'loading' ? t.startedAt : now,
                  rows: finished ? t.est : Math.floor((t.est * done) / t.work),
                  state: finished ? (failed ? 'failed' : 'indexing') : 'loading',
                  ...(failed ? { error: { code: 'QUERY', message: `${t.unmappable} characters could not be converted (on_unmappable = fail)` }, finishedAt: now } : {}),
                });
              }
              // Tables without data (structure only) are done once the structure exists.
              for (const t of sim) if (!t.loads && t.state === 'pending') set(t, { state: 'done', finishedAt: now });
              publish(now);
              return;
            }
            for (const t of sim) if (t.state === 'indexing') set(t, { state: 'done', finishedAt: now });
            for (const t of sim) if (t.state === 'pending') set(t, { state: 'done', finishedAt: now });
            if (phase === 'data' || phase === 'pre_data') {
              phase = 'post_data';
              publish(now);
              return;
            }
            if (phase === 'post_data') {
              phase = 'finalize';
              publish(now);
              return;
            }
            if (phase === 'finalize') {
              phase = 'verify';
              publish(now);
              return;
            }
          }
          ctx.clear(timer);
          ctx.jobs.setState(jobId, 'finishing');
          status.state = 'finishing';
          ctx.after(tick, () => {
            const end = Date.now();
            checkpoint();
            const res = result(end, false);
            const failedTables = res.tables.filter((t) => t.state === 'failed');
            const finalState: JobState = failedTables.length > 0 ? 'failed' : 'done';
            status.state = finalState;
            status.result = res;
            run.result = res;
            run.finishedAt = ctx.nowIso();
            const rows = res.tables.reduce((n, t) => n + Number(t.rowsWritten), 0);
            ctx.log(
              finalState === 'done' ? 'info' : 'error',
              `Copy ${run.label}: ${res.tables.filter((t) => t.state === 'done').length} done, ${failedTables.length} failed, ${res.skippedExisting} skipped${res.problems ? `, ${res.problems} with problems` : ''}${dryRun ? ' (dry run)' : ''}`,
              jobId,
              'bulkdb::copy',
            );
            ctx.jobs.finish(jobId, finalState, {
              result: { rows: BigInt(rows), bytes: BigInt(rows * ROW_BYTES), files: [], durationMs: res.durationMs, notes: [] },
              ...(finalState === 'failed' ? { error: { code: 'QUERY', message: `${failedTables.length} tables failed`, detail: failedTables.map((t) => `${t.targetName}: ${t.error?.message ?? ''}`).join('\n') } } : {}),
            });
          });
        });
      };
      ctx.after(tick, () => {
        if (status.state === 'cancelled') return;
        if (p.blockers.length > 0) {
          // Core `End::Rejected`: no result; the reason is only the job's error.
          const codes = p.blockers.map((b) => b.code);
          status.state = 'rejected';
          run.finishedAt = ctx.nowIso();
          ctx.log('warn', `Copy ${run.label}: rejected (${codes.join(', ')})`, jobId, 'bulkdb::copy');
          ctx.jobs.finish(jobId, 'rejected', {
            error: { code: 'INVALID', message: `the copy plan has blockers: ${codes.join(', ')}`, detail: p.blockers.map((b) => `${b.code}: ${b.message}`).join('\n') },
          });
          return;
        }
        if (missing.length === 0) {
          goRunning();
          return;
        }
        // Core 5.1 step 3: wait in `warned` until `job_resume` names every missing key.
        ctx.jobs.setPreflight(jobId, warnedReport(p, missing));
        ctx.jobs.setState(jobId, 'warned');
        status.state = 'warned';
        ctx.log('info', `Copy ${run.label}: waiting for acknowledgements (${missing.map((m) => m.key).join(', ')})`, jobId, 'bulkdb::copy');
        ctx.jobs.onResume(jobId, (keys) => {
          ctx.log('info', `Copy ${run.label}: acknowledged ${keys.join(', ')}`, jobId, 'bulkdb::copy');
          goRunning();
        });
      });
    });
    return jobId;
  };

  const waitPlan = (token: string | undefined): Promise<void> => {
    const ms = ctx.options().latencyMs * 15;
    if (!token || ms <= 0) return Promise.resolve();
    return new Promise<void>((resolve, rejectPlan) => {
      const h = ctx.after(ms, () => {
        planWaits.delete(token);
        resolve();
      });
      planWaits.set(token, (cancelled) => {
        ctx.clear(h);
        planWaits.delete(token);
        if (cancelled) rejectPlan(ctx.error('CANCELLED', 'Planning cancelled'));
        else resolve();
      });
    });
  };

  const handlers: MockFeature['handlers'] = {
    copy_plan: async (a: MockArgs): Promise<CopyPlan> => {
      const req = a.req as CopyRequest;
      await waitPlan(typeof a.planToken === 'string' ? a.planToken : undefined);
      const p = plan(req, false);
      remember(p);
      return p;
    },
    copy_plan_cancel: (a: MockArgs) => {
      planWaits.get(String(a.planToken))?.(true);
    },
    copy_start: (a: MockArgs): string => {
      const req = a.req as CopyRequest;
      const reviewed = String(a.planId ?? '');
      const acks = new Set((a.acknowledgedWarnings as string[] | undefined) ?? []);
      if (!req.source.database.trim() || !req.target.database.trim()) throw ctx.error('INVALID', 'copy: the source and target databases are required');
      const p = plan(req, true);
      // Core 5.1: blockers end the job `rejected`; otherwise a plan that differs from the
      // reviewed one, or acknowledgements still missing, make the job wait in `warned` (never a
      // refusal of the call).
      const blocked = p.blockers.length > 0;
      if (!blocked && reviewed && p.planId !== reviewed) markPlanChanged(p, reviewed);
      remember(p);
      const missing = blocked ? [] : p.acks.filter((x) => !acks.has(x.key));
      const source = ctx.findConn(req.source.connId);
      const target = ctx.findConn(req.target.connId);
      const run: MockRun = {
        runId: '',
        label: req.name ?? `${source.name}.${req.source.database} → ${target.name}.${req.target.database}`,
        req: storedRequest(req),
        dryRun: req.options.dryRun,
        checkpoints: new Map(),
        jobId: '',
        startedAt: ctx.nowIso(),
        plan: p,
      };
      return launch(run, p, req.options.dryRun, missing);
    },
    copy_resume: (a: MockArgs): string => {
      const r = a.req as CopyResumeRequest;
      const run = runs.get(r.runId);
      if (!run) throw ctx.error('NOT_FOUND', `copy run ${r.runId} not found`);
      if (inUse(run)) throw ctx.error('INVALID', `copy run ${r.runId} is in use (RUN_IN_USE)`);
      if (run.dryRun) throw ctx.error('INVALID', 'dry runs cannot be resumed');
      const req: CopyRequest = { ...run.req, options: { ...run.req.options, dryRun: false, ...(r.confirmProduction ? { confirmProduction: r.confirmProduction } : {}) } };
      const p = plan(req, true, { runId: run.runId, checkpoints: run.checkpoints, partial: r.partial ?? 'fail' });
      // As in core, a resume names its own keys: what is missing makes the new job wait in
      // `warned`; blockers (RESUME_APPEND_PARTIAL, PRODUCTION_CONFIRM, …) end it `rejected`.
      const acked = new Set(r.acknowledged ?? []);
      return launch(run, p, false, p.blockers.length > 0 ? [] : p.acks.filter((x) => !acked.has(x.key)));
    },
    copy_status: (a: MockArgs): CopyStatus => {
      const jobId = String(a.jobId);
      const s = statuses.get(jobId);
      if (!s) throw ctx.error('NOT_FOUND', `copy job ${jobId} not found`);
      return structuredClone({ ...s, state: ctx.jobs.get(jobId)?.state ?? s.state });
    },
    copy_runs: (a: MockArgs): CopyRunInfo[] => {
      const connId = typeof a.connId === 'string' ? a.connId : undefined;
      const limit = typeof a.limit === 'number' ? a.limit : 50;
      return [...runs.values()]
        .filter((r) => !connId || r.req.source.connId === connId || r.req.target.connId === connId)
        .reverse()
        .slice(0, limit)
        .map(info);
    },
    copy_run: (a: MockArgs): CopyRunDetail => {
      const run = runs.get(String(a.runId));
      if (!run) throw ctx.error('NOT_FOUND', `copy run ${String(a.runId)} not found`);
      const result = statuses.get(run.jobId)?.result ?? run.result;
      return structuredClone({ info: info(run), request: run.req, ...(result ? { result } : {}) });
    },
    copy_forget: (a: MockArgs) => {
      const id = String(a.runId);
      const run = runs.get(id);
      if (!run) throw ctx.error('NOT_FOUND', `copy run ${id} not found`);
      if (inUse(run)) throw ctx.error('INVALID', `copy run ${id} is in use (RUN_IN_USE)`);
      runs.delete(id);
      for (const [job, r] of jobRuns) {
        if (r !== id) continue;
        jobRuns.delete(job);
        statuses.delete(job);
      }
    },
  };

  return {
    handlers,
    reset() {
      runs.clear();
      jobRuns.clear();
      statuses.clear();
      planWaits.clear();
      recentPlans.clear();
    },
  };
}
