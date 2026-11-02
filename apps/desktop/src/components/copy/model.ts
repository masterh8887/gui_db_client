// Pure helpers of the DB copy wizard (docs/db-copy.md 7): endpoint validity and captions, the
// start gate, plan summaries and the `seq`-ordered progress merge. No React, no IPC: the store
// (`stores/copy.ts`), the components and the tests share them.

import type {
  AckNeed,
  ColumnTreatment,
  Connection,
  CopyFamily,
  CopyIssue,
  CopyJobInfo,
  CopyOptions,
  CopyPhase,
  CopyPlan,
  CopyProgress,
  CopyRequest,
  CopyServerFacts,
  CopyStatus,
  CopyTableProgress,
  JobState,
  PlanAction,
  PlanItem,
} from 'ipc-types';
import defaults from './copy-options-default.json';
import { cs, k, type CsKey } from './strings';

export const COPY_FAMILIES: readonly CopyFamily[] = ['postgres', 'mysql', 'sqlite'];
export const PHASES: readonly CopyPhase[] = ['plan', 'pre_data', 'data', 'post_data', 'finalize', 'verify'];
export const POLICIES = ['skip', 'truncate', 'drop_recreate', 'append'] as const;
/** Plan actions of a table that loads data. */
export const LOAD_ACTIONS: readonly PlanAction[] = ['create', 'truncate', 'drop_recreate', 'append'];
const TERMINAL: readonly JobState[] = ['done', 'cancelled', 'failed', 'rejected'];
const STATE_RANK: Record<JobState, number> = {
  pending: 0,
  preflight: 1,
  warned: 2,
  running: 3,
  finishing: 4,
  done: 5,
  cancelled: 5,
  failed: 5,
  rejected: 5,
};

export function isTerminal(state: JobState | undefined): boolean {
  return !!state && TERMINAL.includes(state);
}

/** The copy family of a driver, or null when copying it is not supported (`FAMILY_UNSUPPORTED`). */
export function familyOf(driver: string | undefined): CopyFamily | null {
  return driver === 'postgres' || driver === 'mysql' || driver === 'sqlite' ? driver : null;
}

export function driverLabel(driver: string | undefined): string {
  switch (driver) {
    case 'postgres':
      return 'PostgreSQL';
    case 'mysql':
      return 'MySQL';
    case 'sqlite':
      return 'SQLite';
    case 'mssql':
      return 'SQL Server';
    case 'oracle':
      return 'Oracle';
    case 'odbc':
      return 'ODBC';
    default:
      return driver ?? '';
  }
}

/**
 * The non-secret endpoint of one side: "{driver} · {host:port}/{db}[.{schema}]" (+ ssh hop), or
 * "SQLite · {path}". Built from the saved connection only (never a secret).
 */
export function endpointCaption(conn: Connection | undefined, database?: string, schema?: string): string {
  if (!conn) return '';
  if (conn.driver === 'sqlite') return cs('endpointSqlite', { path: conn.fields.path ?? '' });
  const hostPort = `${conn.host ?? 'localhost'}${conn.port ? `:${conn.port}` : ''}`;
  const db = database ? `/${database}${schema ? `.${schema}` : ''}` : '';
  const ssh = conn.ssh?.enabled && conn.ssh.host ? cs('viaSsh', { host: conn.ssh.host }) : '';
  return `${driverLabel(conn.driver)} · ${hostPort}${db}${ssh}`;
}

export type EndpointProblem =
  | 'sourceConn'
  | 'sourceDriver'
  | 'sourceDb'
  | 'targetConn'
  | 'family'
  | 'notWritable'
  | 'targetDb'
  | 'sameDatabase';

/**
 * Why the endpoints of `draft` are not usable yet (null = steps 2-4 are reachable). The same
 * connection with the same database is refused here; core's `SAME_DATABASE` decides for other
 * connections that point at the same server.
 */
export function endpointProblem(draft: CopyRequest, connections: readonly Connection[]): EndpointProblem | null {
  const src = connections.find((c) => c.id === draft.source.connId);
  if (!src) return 'sourceConn';
  if (!familyOf(src.driver)) return 'sourceDriver';
  if (!draft.source.database.trim()) return 'sourceDb';
  const tgt = connections.find((c) => c.id === draft.target.connId);
  if (!tgt) return 'targetConn';
  if (tgt.driver !== src.driver) return 'family';
  if (tgt.writeAllowed !== true) return 'notWritable';
  if (!draft.target.database.trim()) return 'targetDb';
  if (isSameDatabase(draft)) return 'sameDatabase';
  return null;
}

/** Same connection and same database (whatever the schemas): `SAME_DATABASE`. */
export function isSameDatabase(draft: CopyRequest): boolean {
  return !!draft.source.connId && draft.source.connId === draft.target.connId && draft.source.database === draft.target.database;
}

function sortValue(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(sortValue);
  if (v && typeof v === 'object') {
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>)
        .filter(([, x]) => x !== undefined)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([key, x]) => [key, sortValue(x)]),
    );
  }
  return v;
}

/** Deep equality of two JSON-like values (absent and `undefined` fields are equal). */
export function sameJson(a: unknown, b: unknown): boolean {
  return JSON.stringify(sortValue(a)) === JSON.stringify(sortValue(b));
}

/** Whether `options` equals the defaults (step 1 then offers [계획 만들기] directly). */
export function isDefaultOptions(options: CopyOptions): boolean {
  return sameJson(options, defaults);
}

/**
 * Whether the draft carries edits worth confirming before it is discarded (spec 7): a table
 * selection, or non-default options (renames and per-item overrides included). Picking the
 * endpoints alone never counts, and a freshly opened draft is never dirty.
 */
export function isDirtyDraft(draft: CopyRequest): boolean {
  return draft.selection.kind !== 'all' || !isDefaultOptions(draft.options);
}

/** A "pick tables" selection with nothing chosen: an empty, meaningless plan the wizard blocks. */
export function selectionEmpty(draft: CopyRequest): boolean {
  return draft.selection.kind === 'tables' && draft.selection.tables.length === 0;
}

/**
 * `options` with incomplete rename rows (an empty `from` or `to`) dropped, so the generated DDL
 * never names an empty target (spec 7). Returns the same object when nothing is dropped.
 */
export function cleanRenames(options: CopyOptions): CopyOptions {
  const kept = options.rename.filter((m) => m.from.trim() !== '' && m.to.trim() !== '');
  return kept.length === options.rename.length ? options : { ...options, rename: kept };
}

/** Title of an issue: `issue.<CODE>` when known, else the English message from core. */
export function issueTitle(issue: Pick<CopyIssue, 'code' | 'message'>): string {
  const key = `issue.${issue.code}`;
  return (cs.keys as readonly string[]).includes(key) ? cs(k(key)) : issue.message;
}

/** Issues whose remedy is `mysql.columns` (`treat_as`) for the column they name. */
export const COLUMN_TREATMENT: Readonly<Record<string, ColumnTreatment>> = {
  BINARY_ATTR_UNDECODABLE: 'binary',
  BLOB_HOLDS_TEXT: 'text',
};

/**
 * The source column an issue is about: core names it in backquotes in the message ("BLOB column
 * `memo` looks like …"); a single-line `detail` without spaces is taken when the message names
 * none. Undefined when neither does.
 */
export function issueColumn(issue: Pick<CopyIssue, 'message' | 'detail'>): string | undefined {
  const m = /`(.+)`/.exec(issue.message);
  if (m) return m[1];
  const d = issue.detail?.trim();
  return d && !/\s/.test(d) ? d : undefined;
}

/**
 * The recommendation in the UI language: `rec.<CODE>` when known (with `{column}`), else core's
 * English text, which may name API options.
 */
export function recommendationText(issue: Pick<CopyIssue, 'code' | 'message' | 'detail' | 'recommendation'>): string | undefined {
  if (!issue.recommendation) return undefined;
  const key = `rec.${issue.code}`;
  if (!(cs.keys as readonly string[]).includes(key)) return issue.recommendation;
  const column = issueColumn(issue);
  if (COLUMN_TREATMENT[issue.code] && !column) return issue.recommendation;
  return cs(k(key), { column: column ?? '' });
}

/** The lines of a job error's detail; a rejected copy lists "CODE: message" per blocker. */
export function errorLines(detail: string | undefined): { code?: string; text: string }[] {
  if (!detail) return [];
  return detail
    .split('\n')
    .filter((l) => l.trim())
    .map((l) => {
      const m = /^([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*): (.*)$/.exec(l);
      return m ? { code: m[1], text: m[2] } : { text: l };
    });
}

/** Every issue of the plan: plan-level ones, then the items' in item order. */
export function allIssues(plan: CopyPlan): CopyIssue[] {
  return [...plan.warnings, ...plan.items.flatMap((it) => it.warnings.map((w) => ({ ...w, itemId: w.itemId ?? it.id })))];
}

/** The first issue behind an acknowledgement (for its message, detail and recommendation). */
export function ackIssue(plan: CopyPlan, ack: AckNeed): CopyIssue | undefined {
  return allIssues(plan).find((i) => i.code === ack.code);
}

/** `info` issues (never acknowledged), plan-level and per item, one row per code. */
export function infoIssues(plan: CopyPlan): { issue: CopyIssue; count: number }[] {
  const byCode = new Map<string, { issue: CopyIssue; count: number }>();
  for (const i of allIssues(plan)) {
    if (i.severity !== 'info') continue;
    const cur = byCode.get(i.code);
    if (cur) cur.count += 1;
    else byCode.set(i.code, { issue: i, count: 1 });
  }
  return [...byCode.values()];
}

export type StartBlock = 'noPlan' | 'planBusy' | 'starting' | 'endpoints' | 'notWritable' | 'blockers' | 'acks' | 'production';

export interface GateInput {
  plan: CopyPlan | null;
  planBusy: boolean;
  draft: CopyRequest;
  acks: readonly string[];
  confirmName: string;
  starting?: boolean;
}

/**
 * Why [복사 시작] / [모의 실행] stay disabled (null = may start): a plan without blockers, a
 * write-allowed target, every ack key acknowledged and, for a production target, its exact name.
 */
export function startBlock(w: GateInput, connections: readonly Connection[]): StartBlock | null {
  if (w.starting) return 'starting';
  if (w.planBusy) return 'planBusy';
  if (!w.plan) return 'noPlan';
  const target = connections.find((c) => c.id === w.draft.target.connId);
  if (!target || target.writeAllowed !== true) return 'notWritable';
  const problem = endpointProblem(w.draft, connections);
  if (problem) return 'endpoints';
  if (w.plan.blockers.length > 0) return 'blockers';
  if (w.plan.acks.some((a) => !w.acks.includes(a.key))) return 'acks';
  if (target.tag === 'production' && w.confirmName !== target.name) return 'production';
  return null;
}

/**
 * Gate of [계속] for a warned copy job: only its blockers and acknowledgements matter. `pending`,
 * when given, are the ack keys core is still waiting for (its preflight lists one warning per
 * missing key); acknowledgements already given at start then do not have to be re-ticked.
 */
export function continueBlock(plan: CopyPlan | null | undefined, acks: readonly string[], pending?: ReadonlySet<string>): StartBlock | null {
  if (!plan) return 'noPlan';
  if (plan.blockers.length > 0) return 'blockers';
  const required = pending ? plan.acks.filter((a) => pending.has(a.key)) : plan.acks;
  if (required.some((a) => !acks.includes(a.key))) return 'acks';
  return null;
}

/**
 * The ack keys a warned job still waits for, from its preflight report (core sets one warning per
 * missing key, whose `code` is the ack key). Undefined when the preflight is not known yet, so
 * every ack still counts.
 */
export function pendingAckKeys(warnings: readonly { code: string }[] | undefined): ReadonlySet<string> | undefined {
  return warnings ? new Set(warnings.map((w) => w.code)) : undefined;
}

export function startBlockText(b: StartBlock): string {
  return cs(k(`reason.${b}`));
}

/** Items in execution order: phase, then `order`. */
export function sortedItems(plan: CopyPlan): PlanItem[] {
  return [...plan.items].sort((a, b) => PHASES.indexOf(a.phase) - PHASES.indexOf(b.phase) || a.order - b.order);
}

/** Items named by `PLAN_CHANGED` (its detail lists the changed item ids). */
export function planChangedIds(plan: CopyPlan | null | undefined): Set<string> {
  const out = new Set<string>();
  if (!plan) return out;
  const detail = allIssues(plan)
    .filter((i) => i.code === 'PLAN_CHANGED')
    .map((i) => i.detail ?? '')
    .join('\n');
  if (!detail) return out;
  for (const it of plan.items) if (detail.includes(it.id)) out.add(it.id);
  return out;
}

export function itemIssues(plan: CopyPlan, itemId: string): CopyIssue[] {
  const item = plan.items.find((i) => i.id === itemId);
  return [...(item?.warnings ?? []), ...plan.warnings.filter((w) => w.itemId === itemId), ...plan.blockers.filter((b) => b.itemId === itemId)];
}

/** Whether the item is an existing table whose policy the 작업 menu can change. */
export function hasPolicyMenu(item: PlanItem): boolean {
  return item.kind === 'table' && item.existsInTarget && ['skip_existing', 'truncate', 'drop_recreate', 'append'].includes(item.action);
}

export function actionPolicy(action: PlanAction): (typeof POLICIES)[number] | null {
  if (action === 'skip_existing') return 'skip';
  return action === 'truncate' || action === 'drop_recreate' || action === 'append' ? action : null;
}

/** "있음 · 데이터 있음" / "있음 · 비어 있음" / "있음" / null for new objects. */
export function targetStateKey(item: PlanItem): CsKey | null {
  if (!item.existsInTarget) return null;
  if (item.targetHasRows === true) return 'exists.rows';
  if (item.targetHasRows === false) return 'exists.empty';
  return 'exists.yes';
}

export interface PlanCounts {
  existing: number;
  existingEmpty: number;
  loadRows: number;
}

export function existingCounts(plan: CopyPlan): PlanCounts {
  const tables = plan.items.filter((i) => i.kind === 'table');
  const existing = tables.filter((i) => i.existsInTarget);
  const loadRows = tables.filter((i) => LOAD_ACTIONS.includes(i.action) && i.hasData).reduce((n, i) => n + Number(i.estRows ?? 0), 0);
  return { existing: existing.length, existingEmpty: existing.filter((i) => i.targetHasRows === false).length, loadRows };
}

/** The facts line of the plan (target side): version, charset, engine, load method, chunk, snapshot, sessions. */
export function factsParts(plan: CopyPlan, formatBytes: (n: number) => string): string[] {
  const e = plan.effective;
  const parts: string[] = [];
  parts.push(`${driverLabel(plan.target.driver)} ${plan.target.version}`);
  const cc = [e.charset, e.collation].filter(Boolean).join(' / ');
  if (cc) parts.push(cc);
  if (e.engine) parts.push(e.engine);
  if (e.zeroDates) parts.push(cs('zeroDatesFact', { policy: cs(k(`zero.${e.zeroDates}`)) }));
  if (e.loadMethod === 'mysql_insert') parts.push(cs('facts.localInfileOff'));
  else if (e.loadMethod === 'mysql_load_data') parts.push(cs('facts.loadData'));
  else if (e.loadMethod === 'pg_copy_csv') parts.push(cs('facts.pgCopy'));
  else parts.push(cs('facts.sqliteInsert'));
  if (e.chunkBytes !== undefined) parts.push(cs('facts.chunk', { size: formatBytes(Number(e.chunkBytes)) }));
  parts.push(e.consistentSnapshot ? cs('facts.snapshot') : cs('facts.noSnapshot'));
  parts.push(cs('facts.sessions', { n: e.sourceSessions, m: e.targetSessions }));
  return parts;
}

/**
 * "host:port/db[.schema]" from server facts, keeping an ssh hop at the end
 * ("host:port/db via ssh bastion"); SQLite: the path.
 */
export function factsEndpoint(f: Pick<CopyServerFacts, 'driver' | 'endpoint' | 'database'>, schema?: string): string {
  if (f.driver === 'sqlite') return f.endpoint;
  const at = f.endpoint.indexOf(' via ssh ');
  const db = `/${f.database}${schema ? `.${schema}` : ''}`;
  return at < 0 ? `${f.endpoint}${db}` : `${f.endpoint.slice(0, at)}${db}${f.endpoint.slice(at)}`;
}

/** Whether a server version names an NDB Cluster build (`5.7.44-ndb-7.6.29`). */
export function isNdb(version: string | undefined): boolean {
  return !!version && version.includes('-ndb-');
}

// ---- progress --------------------------------------------------------------------------------

export function emptyProgress(jobId: string, runId: string): CopyProgress {
  return {
    jobId,
    runId,
    seq: BigInt(0),
    phase: 'plan',
    itemsTotal: 0,
    itemsDone: 0,
    itemsFailed: 0,
    itemsSkipped: 0,
    tablesTotal: 0,
    tablesDone: 0,
    tables: [],
    full: true,
  };
}

/**
 * Merge a progress event (delta or full) into what the store holds: each table entry and the
 * job-level fields are replaced only by a newer `seq`, so a late full status never overwrites
 * newer values and a lost delta is repaired by the next full one.
 */
export function mergeProgress(prev: CopyProgress, next: CopyProgress): CopyProgress {
  const tables: CopyTableProgress[] = [...prev.tables];
  const index = new Map(tables.map((t, i) => [t.itemId, i]));
  for (const t of next.tables) {
    const i = index.get(t.itemId);
    if (i === undefined) {
      index.set(t.itemId, tables.length);
      tables.push(t);
    } else if (Number(t.seq) > Number(tables[i].seq)) {
      tables[i] = t;
    }
  }
  const head = Number(next.seq) > Number(prev.seq) ? next : prev;
  return { ...head, tables, full: true };
}

/** Merge a `copy_status` answer: the state never moves back, plan and result are kept once known. */
export function mergeStatus(prev: CopyStatus | undefined, next: CopyStatus): CopyStatus {
  if (!prev) return { ...next, progress: { ...next.progress, full: true } };
  const state = STATE_RANK[next.state] >= STATE_RANK[prev.state] && !isTerminal(prev.state) ? next.state : prev.state;
  return {
    ...prev,
    ...next,
    state,
    plan: next.plan ?? prev.plan,
    result: next.result ?? prev.result,
    progress: mergeProgress(prev.progress, next.progress),
  };
}

/** Table counts of a copy job from its `copy:progress` events (kept for every copy job). */
export interface CopyLive {
  seq: number;
  tablesTotal: number;
  tablesDone: number;
  /** Tables that ended failed or cancelled (core's `tables_failed`). */
  failed: string[];
}

const ENDED_BAD = (s: CopyTableProgress['state']) => s === 'failed' || s === 'cancelled';

/**
 * Fold a progress event into the counts: the job-level counts follow the newest `seq`; a table
 * that ended failed or cancelled stays counted. Returns `prev` itself when nothing changed.
 */
export function mergeLive(prev: CopyLive | undefined, p: CopyProgress): CopyLive {
  const seq = Number(p.seq);
  const failed = prev ? [...prev.failed] : [];
  for (const t of p.tables) if (ENDED_BAD(t.state) && !failed.includes(t.itemId)) failed.push(t.itemId);
  const next: CopyLive = !prev || seq >= prev.seq ? { seq, tablesTotal: p.tablesTotal, tablesDone: p.tablesDone, failed } : { ...prev, failed };
  if (prev && next.seq === prev.seq && next.tablesTotal === prev.tablesTotal && next.tablesDone === prev.tablesDone && failed.length === prev.failed.length) return prev;
  return next;
}

/**
 * Tables done / total / failed of an Activity copy row: the largest of what `job_get` said
 * (`JobSummary.copy`), the status this window shows and the live counts. Every count only grows
 * within one job, so the largest is the newest.
 */
export function copyCounts(copy: CopyJobInfo, shown: CopyProgress | undefined, live: CopyLive | undefined): { done: number; total: number; failed: number } {
  const shownFailed = shown ? shown.tables.filter((t) => ENDED_BAD(t.state)).length : 0;
  return {
    done: Math.max(copy.tablesDone, shown?.tablesDone ?? 0, live?.tablesDone ?? 0),
    total: Math.max(copy.tablesTotal, shown?.tablesTotal ?? 0, live?.tablesTotal ?? 0),
    failed: Math.max(copy.tablesFailed, shownFailed, live?.failed.length ?? 0),
  };
}

export type TableFilter = 'all' | 'active' | 'failed' | 'done';

export function matchesFilter(t: CopyTableProgress, f: TableFilter): boolean {
  switch (f) {
    case 'active':
      return t.state === 'creating' || t.state === 'loading' || t.state === 'indexing' || t.state === 'verifying' || t.state === 'pending';
    case 'failed':
      return t.state === 'failed' || t.state === 'cancelled';
    case 'done':
      return t.state === 'done' || t.state === 'skipped' || t.state === 'planned';
    default:
      return true;
  }
}

/** Overall progress 0..1 from rows written against the estimates (undefined: unknown). */
export function overallFraction(p: CopyProgress | undefined): number | undefined {
  if (!p || p.tables.length === 0) return undefined;
  let est = 0;
  let done = 0;
  for (const t of p.tables) {
    const e = Number(t.estRows ?? 0);
    const finished = t.state === 'done' || t.state === 'skipped' || t.state === 'planned';
    est += e;
    done += finished ? e : Math.min(e, Number(t.rowsWritten));
  }
  if (est <= 0) return p.tablesTotal > 0 ? p.tablesDone / p.tablesTotal : undefined;
  return Math.max(0, Math.min(1, done / est));
}
