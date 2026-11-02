'use client';

// Query documents, one per query target tab (DESIGN.md §7.1 "extract"): SQL, session and output
// options, the last plan / check / preflight, and the preflight sheet (with a queue). Job state
// after `export_start` belongs to the jobs store (Rust is the state of record).
//
// Async attachment rule: every action takes `tabId`, reads the doc and its `gen` once at the
// start, and writes only through `patch(tabId, gen, partial)`, which is a no-op when the doc is
// gone or `gen` changed (a closed-and-reopened tab never receives a stale result). No action
// reads "the active tab" after an `await`. A check or plan is kept only while the doc's text
// still equals what was sent.

import type {
  CsvEncoding,
  CsvOpts,
  ExportRequest,
  HistoryEntry,
  OutputOpts,
  ParquetOpts,
  PlanSummary,
  PreflightReport,
  SessionOpts,
  SplitOpts,
  SqlCheck,
  SqlFile,
  Connection,
} from 'ipc-types';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';
import { exportPreflight, exportStart, queryExplain, sqlCheck, sqlFileRead } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { t } from '@/lib/i18n';
import { isWindows } from '@/lib/platform';
import { needsSessionPassword, useConnectionsStore } from './connections';
import { useJobsStore } from './jobs';
import { logLocal } from './logs';
import { useSettingsStore } from './settings';
import { useUiStore } from './ui';

export type SqlMode = 'text' | 'file';
export type ExportPanelSection = 'output' | 'session' | 'plan' | 'check' | 'job';
export type ExtractBusy = 'check' | 'preflight' | 'starting' | 'explain' | 'file' | 'select' | null;

export interface QueryDoc {
  tabId: string;
  /** Fixed at creation. */
  connId: string;
  /** Bumped by `createDoc` / `restoreDoc`; async results carry the gen they started with. */
  gen: number;
  sqlMode: SqlMode;
  sql: string;
  /** Dirty = `sql !== baseline`. */
  baseline: string;
  sqlFile: string;
  sqlFileInfo: SqlFile | null;
  name: string;
  session: SessionOpts;
  output: OutputOpts;
  lastPreflight: PreflightReport | null;
  lastPlan: PlanSummary | null;
  lastCheck: SqlCheck | null;
  busy: ExtractBusy;
  error: string | null;
  lastJobId: string | null;
  panel: ExportPanelSection;
  panelCollapsed: boolean;
}

export interface QueryDocInit {
  sql?: string;
  sqlMode?: SqlMode;
  sqlFile?: string;
  name?: string;
  session?: SessionOpts;
  output?: Partial<OutputOpts>;
}

export type PreflightDialogState =
  | { kind: 'run'; tabId: string; req: ExportRequest; report: PreflightReport; busy: boolean; error: string | null }
  | { kind: 'resume'; jobId: string; report: PreflightReport; busy: boolean; error: string | null };

export const DEFAULT_NAME_PATTERN = '{conn}_{name}_{yyyyMMdd_HHmm}';
/** Excel's sheet limit is 1,048,576 rows; split below it. */
export const EXCEL_SPLIT_ROWS = 1_000_000;

export function defaultCsvOpts(crlf = isWindows(), bom = isWindows()): CsvOpts {
  return { header: true, delimiter: ',', nullAs: '', bom, crlf };
}

/** The file encoding of CSV options (absent = UTF-8, docs/mysql-compat.md §6). */
export function csvEncoding(csv: CsvOpts | undefined): CsvEncoding {
  return csv?.encoding === 'euc-kr' ? 'euc-kr' : 'utf-8';
}

/**
 * CSV options with the encoding applied: UTF-8 drops the `encoding` key (requests stay minimal
 * and older cores read them unchanged); EUC-KR clears the BOM, which only exists in UTF-8.
 */
export function withCsvEncoding(csv: CsvOpts, encoding: CsvEncoding): CsvOpts {
  const base: CsvOpts = { header: csv.header, delimiter: csv.delimiter, nullAs: csv.nullAs, bom: csv.bom, crlf: csv.crlf };
  return encoding === 'euc-kr' ? { ...base, bom: false, encoding } : base;
}

/**
 * Output options with the CSV encoding rules applied (`withCsvEncoding`). Options seeded from
 * outside the store (history entries, the CLI's `--excel --encoding euc-kr`) can hold
 * `bom: true` with EUC-KR, which the core never writes (`CsvOpts::writes_bom`).
 */
export function normalizeOutput(output: OutputOpts): OutputOpts {
  return output.csv ? { ...output, csv: withCsvEncoding(output.csv, csvEncoding(output.csv)) } : output;
}

export function defaultParquetOpts(): ParquetOpts {
  return { compression: 'zstd', rowGroupMb: 128 };
}

export function defaultOutputOpts(): OutputOpts {
  return {
    dir: '',
    namePattern: DEFAULT_NAME_PATTERN,
    format: 'csv',
    csv: defaultCsvOpts(),
    parquet: defaultParquetOpts(),
    overwrite: false,
  };
}

export function isDocDirty(d: QueryDoc): boolean {
  return d.sql !== d.baseline;
}

/** The SQL text a doc would send (inline text, or the loaded file's content). */
function docText(d: QueryDoc): string {
  return d.sqlMode === 'text' ? d.sql : (d.sqlFileInfo?.content ?? '');
}

/** Identity of the text a result was computed for. */
function textKey(d: QueryDoc): string {
  return d.sqlMode === 'text' ? `t:${d.sql}` : `f:${d.sqlFile}:${d.sqlFileInfo?.content ?? ''}`;
}

export interface ExtractState {
  docs: Record<string, QueryDoc>;
  /** The open preflight sheet. */
  preflightDialog: PreflightDialogState | null;
  /** Reports that arrived while a sheet was open, oldest first. */
  preflightQueue: PreflightDialogState[];

  createDoc(tabId: string, connId: string, init?: QueryDocInit): QueryDoc;
  disposeDoc(tabId: string): void;
  /** Put back a closed tab's doc: `busy` cleared, a new `gen`. */
  restoreDoc(doc: QueryDoc): void;

  setSqlMode(tabId: string, mode: SqlMode): void;
  setSql(tabId: string, sql: string): void;
  setSqlFile(tabId: string, path: string): void;
  setName(tabId: string, name: string): void;
  setSession(tabId: string, patch: Partial<SessionOpts>): void;
  setOutput(tabId: string, patch: Partial<OutputOpts>): void;
  setCsv(tabId: string, patch: Partial<CsvOpts>): void;
  /** CSV file encoding: EUC-KR clears the BOM; UTF-8 removes the key (see `withCsvEncoding`). */
  setCsvEncoding(tabId: string, encoding: CsvEncoding): void;
  setParquet(tabId: string, patch: Partial<ParquetOpts>): void;
  setSplit(tabId: string, split: SplitOpts | undefined): void;
  applyExcelPreset(tabId: string): void;
  /** Show a panel section (also expands the panel). */
  setPanel(tabId: string, section: ExportPanelSection): void;
  togglePanel(tabId: string): void;
  clearError(tabId: string): void;

  /** `null` (and an error on the doc) when the doc cannot run (no SQL). */
  buildRequest(tabId: string): ExportRequest | null;
  /** SQL guard on the doc's text with its connection's dialect; expands the panel on 검사. */
  check(tabId: string): Promise<SqlCheck | null>;
  /** SQL guard on `sql` (live check) without touching busy / lastCheck; `null` on failure. */
  checkSql(tabId: string, sql: string): Promise<SqlCheck | null>;
  /** Preflight, then start or open (or queue) the sheet. */
  run(tabId: string): Promise<void>;
  explain(tabId: string, analyze?: boolean): Promise<void>;
  loadSqlFile(tabId: string, path?: string): Promise<void>;
  /** Start (run sheet: the captured request) or resume (resume sheet) with these acks. */
  startAcknowledged(acks: string[]): Promise<string | null>;
  openResumeDialog(jobId: string, report: PreflightReport): void;
  /** Close the sheet; the next queued one opens. */
  closeDialog(): void;
}

// ---- legacy single-document API (removed in W3) ----------------------------------------------
//
// The pre-redesign panels (`extract/ExtractTab`, the old tree, table and history tabs, the old
// password dialog) call the flat API below. It forwards to the doc of the tab bound with
// `bindLegacyDoc(tabId)` (the QueryTabView stub binds the active tab), or to a private
// `__legacy` doc when nothing is bound; the flat fields mirror that doc. New code never uses it.
export const LEGACY_DOC_ID = '__legacy';

// Declaration merging: these signatures become overloads of the methods above.
export interface ExtractState {
  /** @deprecated Tab whose doc the legacy flat API edits. */
  legacyTabId: string | null;
  /** @deprecated */
  bindLegacyDoc(tabId: string | null): void;
  /** @deprecated mirror of the bound doc */ connId: string | null;
  /** @deprecated */ sqlMode: SqlMode;
  /** @deprecated */ sql: string;
  /** @deprecated */ sqlFile: string;
  /** @deprecated */ sqlFileInfo: SqlFile | null;
  /** @deprecated */ name: string;
  /** @deprecated */ session: SessionOpts;
  /** @deprecated */ output: OutputOpts;
  /** @deprecated */ lastPreflight: PreflightReport | null;
  /** @deprecated */ lastPlan: PlanSummary | null;
  /** @deprecated */ lastCheck: SqlCheck | null;
  /** @deprecated */ busy: ExtractBusy;
  /** @deprecated */ error: string | null;
  /** @deprecated */ lastJobId: string | null;
  /** @deprecated */ setConnId(connId: string | null): void;
  /** @deprecated */ setSqlMode(mode: SqlMode): void;
  /** @deprecated */ setSql(sql: string): void;
  /** @deprecated */ setSqlFile(path: string): void;
  /** @deprecated */ setName(name: string): void;
  /** @deprecated */ setSession(patch: Partial<SessionOpts>): void;
  /** @deprecated */ setOutput(patch: Partial<OutputOpts>): void;
  /** @deprecated */ setCsv(patch: Partial<CsvOpts>): void;
  /** @deprecated */ setParquet(patch: Partial<ParquetOpts>): void;
  /** @deprecated */ setSplit(split: SplitOpts | undefined): void;
  /** @deprecated */ applyExcelPreset(): void;
  /** @deprecated */ applyConnectionDefaults(conn: Connection | undefined): void;
  /** @deprecated */ clearError(): void;
  /** @deprecated */ effectiveConnId(): string | null;
  /** @deprecated */ buildRequest(): ExportRequest | null;
  /** @deprecated */ check(): Promise<SqlCheck | null>;
  /** @deprecated */ checkSql(sql: string): Promise<SqlCheck | null>;
  /** @deprecated */ run(): Promise<void>;
  /** @deprecated */ explain(analyze?: boolean): Promise<void>;
  /** @deprecated */ loadSqlFile(path?: string): Promise<void>;
  /** @deprecated */ loadFromHistory(entry: HistoryEntry): void;
}

let genCounter = 0;
const nextGen = () => (genCounter += 1);

function mirrorOf(doc: QueryDoc | undefined, dialog: PreflightDialogState | null) {
  return {
    connId: doc?.connId ?? null,
    sqlMode: doc?.sqlMode ?? 'text',
    sql: doc?.sql ?? '',
    sqlFile: doc?.sqlFile ?? '',
    sqlFileInfo: doc?.sqlFileInfo ?? null,
    name: doc?.name ?? '',
    session: doc?.session ?? {},
    output: doc?.output ?? defaultOutputOpts(),
    lastPreflight: doc?.lastPreflight ?? null,
    lastPlan: doc?.lastPlan ?? null,
    lastCheck: doc?.lastCheck ?? null,
    busy: dialog?.busy ? ('starting' as const) : (doc?.busy ?? null),
    error: dialog?.error ?? doc?.error ?? null,
    lastJobId: doc?.lastJobId ?? null,
  } satisfies Partial<ExtractState>;
}

export const useExtractStore = create<ExtractState>()((set, get) => {
  const findConn = (connId: string) => useConnectionsStore.getState().connections.find((c) => c.id === connId);

  /** Write `partial` into doc `tabId` only while it exists with generation `gen`. */
  const patch = (tabId: string, gen: number, partial: Partial<QueryDoc> | ((d: QueryDoc) => Partial<QueryDoc>)) =>
    set((s) => {
      const d = s.docs[tabId];
      if (!d || d.gen !== gen) return s;
      const p = typeof partial === 'function' ? partial(d) : partial;
      return { docs: { ...s.docs, [tabId]: { ...d, ...p } } };
    });

  /** Synchronous user edit of an existing doc. */
  const edit = (tabId: string, fn: (d: QueryDoc) => Partial<QueryDoc> | null) =>
    set((s) => {
      const d = s.docs[tabId];
      if (!d) return s;
      const p = fn(d);
      return p ? { docs: { ...s.docs, [tabId]: { ...d, ...p } } } : s;
    });

  const alive = (tabId: string, gen: number) => get().docs[tabId]?.gen === gen;
  const clearBusy = (tabId: string, gen: number, kind: ExtractBusy) =>
    patch(tabId, gen, (d) => (d.busy === kind ? { busy: null } : {}));

  const openOrQueue = (dialog: PreflightDialogState) =>
    set((s) => (s.preflightDialog ? { preflightQueue: [...s.preflightQueue, dialog] } : { preflightDialog: dialog }));
  const nextDialog = () =>
    set((s) => ({ preflightDialog: s.preflightQueue[0] ?? null, preflightQueue: s.preflightQueue.slice(1) }));

  // ---- the per-tab API --------------------------------------------------------------------

  const api = {
    createDoc: (tabId: string, connId: string, init: QueryDocInit = {}): QueryDoc => {
      const conn = findConn(connId);
      const settings = useSettingsStore.getState().settings;
      const crlf = settings?.csvCrlfDefault ?? isWindows();
      const sql = init.sql ?? '';
      const doc: QueryDoc = {
        tabId,
        connId,
        gen: nextGen(),
        sqlMode: init.sqlMode ?? (init.sqlFile ? 'file' : 'text'),
        sql,
        baseline: sql,
        sqlFile: init.sqlFile ?? '',
        sqlFileInfo: null,
        name: init.name ?? '',
        session: init.session ? { ...init.session } : conn ? { ...conn.session } : {},
        output: normalizeOutput({
          ...defaultOutputOpts(),
          dir: conn?.outputDir ?? settings?.defaultOutputDir ?? '',
          csv: defaultCsvOpts(crlf),
          ...init.output,
        }),
        lastPreflight: null,
        lastPlan: null,
        lastCheck: null,
        busy: null,
        error: null,
        lastJobId: null,
        panel: 'output',
        panelCollapsed: true,
      };
      set((s) => ({ docs: { ...s.docs, [tabId]: doc } }));
      return doc;
    },

    disposeDoc: (tabId: string) =>
      set((s) => {
        if (!s.docs[tabId]) return s;
        const docs = { ...s.docs };
        delete docs[tabId];
        return { docs, legacyTabId: s.legacyTabId === tabId ? null : s.legacyTabId };
      }),

    restoreDoc: (doc: QueryDoc) => set((s) => ({ docs: { ...s.docs, [doc.tabId]: { ...doc, busy: null, gen: nextGen() } } })),

    // A plan or check belongs to the exact text it was made for: drop both when the text changes.
    setSqlMode: (tabId: string, mode: SqlMode) =>
      edit(tabId, (d) => (d.sqlMode === mode ? null : { sqlMode: mode, lastCheck: null, lastPlan: null })),
    setSql: (tabId: string, sql: string) =>
      edit(tabId, (d) => (d.sql === sql ? null : { sql, lastCheck: null, lastPlan: null, error: null })),
    setSqlFile: (tabId: string, path: string) =>
      edit(tabId, (d) => (d.sqlFile === path ? null : { sqlFile: path, sqlFileInfo: null, lastCheck: null, lastPlan: null })),
    setName: (tabId: string, name: string) => edit(tabId, () => ({ name })),
    setSession: (tabId: string, p: Partial<SessionOpts>) => edit(tabId, (d) => ({ session: { ...d.session, ...p } })),
    setOutput: (tabId: string, p: Partial<OutputOpts>) => edit(tabId, (d) => ({ output: { ...d.output, ...p } })),
    setCsv: (tabId: string, p: Partial<CsvOpts>) =>
      edit(tabId, (d) => ({ output: { ...d.output, csv: { ...(d.output.csv ?? defaultCsvOpts()), ...p } } })),
    setCsvEncoding: (tabId: string, encoding: CsvEncoding) =>
      edit(tabId, (d) => ({ output: { ...d.output, csv: withCsvEncoding(d.output.csv ?? defaultCsvOpts(), encoding) } })),
    setParquet: (tabId: string, p: Partial<ParquetOpts>) =>
      edit(tabId, (d) => ({ output: { ...d.output, parquet: { ...(d.output.parquet ?? defaultParquetOpts()), ...p } } })),
    setSplit: (tabId: string, split: SplitOpts | undefined) => edit(tabId, (d) => ({ output: { ...d.output, split } })),
    applyExcelPreset: (tabId: string) =>
      edit(tabId, (d) => ({
        output: {
          ...d.output,
          format: 'csv',
          // UTF-8 (no `encoding` key) + BOM: Excel detects the encoding from the BOM.
          csv: { header: true, delimiter: ',', nullAs: '', bom: true, crlf: true },
          compress: undefined,
          split: { rows: BigInt(EXCEL_SPLIT_ROWS) },
        },
      })),
    setPanel: (tabId: string, section: ExportPanelSection) => edit(tabId, () => ({ panel: section, panelCollapsed: false })),
    togglePanel: (tabId: string) => edit(tabId, (d) => ({ panelCollapsed: !d.panelCollapsed })),
    clearError: (tabId: string) => edit(tabId, (d) => (d.error ? { error: null } : null)),

    buildRequest: (tabId: string): ExportRequest | null => {
      const d = get().docs[tabId];
      if (!d) return null;
      if (!d.connId) {
        edit(tabId, () => ({ error: t('extract.noConnection') }));
        return null;
      }
      const sql = d.sqlMode === 'text' ? d.sql.trim() : '';
      const sqlFile = d.sqlMode === 'file' ? d.sqlFile.trim() : '';
      if (!sql && !sqlFile) {
        edit(tabId, () => ({ error: t('extract.emptySql') }));
        return null;
      }
      const output: OutputOpts = {
        ...d.output,
        csv: d.output.format === 'csv' ? withCsvEncoding(d.output.csv ?? defaultCsvOpts(), csvEncoding(d.output.csv)) : undefined,
        parquet: d.output.format === 'parquet' ? (d.output.parquet ?? defaultParquetOpts()) : undefined,
        compress: d.output.format === 'csv' ? d.output.compress : undefined,
      };
      return {
        connId: d.connId,
        sql: sql || undefined,
        sqlFile: sqlFile || undefined,
        name: d.name.trim() || undefined,
        session: { ...d.session },
        output,
      };
    },

    check: async (tabId: string): Promise<SqlCheck | null> => {
      const d = get().docs[tabId];
      if (!d) return null;
      const { gen, connId } = d;
      const sent = textKey(d);
      patch(tabId, gen, { busy: 'check', error: null });
      try {
        const result = await sqlCheck(docText(d), connId);
        patch(tabId, gen, (cur) => (textKey(cur) === sent ? { lastCheck: result, panel: 'check', panelCollapsed: false } : {}));
        return result;
      } catch (e) {
        patch(tabId, gen, { error: errorText(e) });
        return null;
      } finally {
        clearBusy(tabId, gen, 'check');
      }
    },

    checkSql: async (tabId: string, sql: string): Promise<SqlCheck | null> => {
      const d = get().docs[tabId];
      if (!d) return null;
      try {
        return await sqlCheck(sql, d.connId);
      } catch {
        return null;
      }
    },

    run: async (tabId: string): Promise<void> => {
      const d = get().docs[tabId];
      if (!d || d.busy) return;
      const req = api.buildRequest(tabId);
      if (!req) return;
      if (needsSessionPassword(useConnectionsStore.getState(), d.connId)) {
        // The password dialog runs `run(tabId)` again once the backend accepted the password.
        useUiStore.getState().askDbPassword(d.connId, 'run', { tabId });
        return;
      }
      const { gen } = d;
      const sentSql = d.sql;
      patch(tabId, gen, { busy: 'preflight', error: null });
      try {
        const report = await exportPreflight(req);
        if (!alive(tabId, gen)) {
          logLocal('info', `preflight finished for a closed tab (${req.name ?? tabId}); nothing started`);
          return;
        }
        patch(tabId, gen, { lastPreflight: report });
        if (report.blockers.length > 0 || report.warnings.length > 0) {
          openOrQueue({ kind: 'run', tabId, req, report, busy: false, error: null });
          return;
        }
        patch(tabId, gen, { busy: 'starting' });
        const jobId = await exportStart(req, []);
        useJobsStore.getState().markLocal(jobId);
        patch(tabId, gen, { lastJobId: jobId, panel: 'job', panelCollapsed: false, baseline: sentSql });
      } catch (e) {
        patch(tabId, gen, { error: errorText(e) });
      } finally {
        patch(tabId, gen, (cur) => (cur.busy === 'preflight' || cur.busy === 'starting' ? { busy: null } : {}));
      }
    },

    explain: async (tabId: string, analyze = false): Promise<void> => {
      const d = get().docs[tabId];
      if (!d || d.busy) return;
      if (!d.connId) {
        edit(tabId, () => ({ error: t('extract.noConnection') }));
        return;
      }
      const sql = docText(d).trim();
      if (!sql) {
        edit(tabId, () => ({ error: t('extract.emptySql') }));
        return;
      }
      if (needsSessionPassword(useConnectionsStore.getState(), d.connId)) {
        useUiStore.getState().askDbPassword(d.connId, 'explain', { tabId });
        return;
      }
      const { gen, connId } = d;
      const sent = textKey(d);
      patch(tabId, gen, { busy: 'explain', error: null });
      try {
        const plan = await queryExplain(connId, sql, analyze);
        patch(tabId, gen, (cur) => (textKey(cur) === sent ? { lastPlan: plan, panel: 'plan', panelCollapsed: false } : {}));
      } catch (e) {
        patch(tabId, gen, { error: errorText(e) });
      } finally {
        clearBusy(tabId, gen, 'explain');
      }
    },

    loadSqlFile: async (tabId: string, path?: string): Promise<void> => {
      const d = get().docs[tabId];
      if (!d) return;
      const p = (path ?? d.sqlFile).trim();
      if (!p) return;
      const { gen } = d;
      patch(tabId, gen, { busy: 'file', error: null, sqlFile: p });
      try {
        const info = await sqlFileRead(p);
        patch(tabId, gen, (cur) =>
          cur.sqlFile === p ? { sqlFileInfo: info, sqlMode: 'file', lastCheck: null, lastPlan: null, baseline: cur.sql } : {},
        );
      } catch (e) {
        patch(tabId, gen, { error: errorText(e), sqlFileInfo: null });
      } finally {
        clearBusy(tabId, gen, 'file');
      }
    },

    startAcknowledged: async (acks: string[]): Promise<string | null> => {
      const dialog = get().preflightDialog;
      if (!dialog) return null;
      // Only the sheet this call belongs to (identified by its report) is updated.
      const setDialog = (next: PreflightDialogState) =>
        set((s) => (s.preflightDialog?.report === dialog.report ? { preflightDialog: next } : s));
      setDialog({ ...dialog, busy: true, error: null });
      try {
        if (dialog.kind === 'resume') {
          await useJobsStore.getState().resume(dialog.jobId, acks);
          nextDialog();
          return dialog.jobId;
        }
        // The user confirmed: the job starts even if the tab was closed meanwhile.
        const jobId = await exportStart(dialog.req, acks);
        useJobsStore.getState().markLocal(jobId);
        const doc = get().docs[dialog.tabId];
        if (doc) patch(dialog.tabId, doc.gen, { lastJobId: jobId, panel: 'job', panelCollapsed: false, baseline: doc.sql });
        nextDialog();
        return jobId;
      } catch (e) {
        setDialog({ ...dialog, busy: false, error: errorText(e) });
        return null;
      }
    },

    openResumeDialog: (jobId: string, report: PreflightReport) =>
      openOrQueue({ kind: 'resume', jobId, report, busy: false, error: null }),

    closeDialog: () => nextDialog(),
  };

  // ---- legacy flat API (removed in W3) -----------------------------------------------------

  /** Text of the private legacy doc, or of the flat fields before it exists (tests set them). */
  const legacySeed = (): QueryDocInit => {
    const s = get();
    const cur = s.docs[LEGACY_DOC_ID];
    if (cur) return { sql: cur.sql, sqlMode: cur.sqlMode, sqlFile: cur.sqlFile, name: cur.name };
    return {
      sql: s.sql,
      sqlMode: s.sqlMode,
      sqlFile: s.sqlFile,
      name: s.name,
      session: Object.keys(s.session).length > 0 ? s.session : undefined,
    };
  };

  /** (Re-)create the private legacy doc for `connId` and bind it. */
  const retargetLegacy = (connId: string, init?: QueryDocInit) => {
    api.createDoc(LEGACY_DOC_ID, connId, init ?? legacySeed());
    set({ legacyTabId: LEGACY_DOC_ID });
  };

  /** The doc the legacy API edits: the bound tab's, else the private `__legacy` doc. */
  const legacyTarget = (): string => {
    const s = get();
    if (s.legacyTabId && s.docs[s.legacyTabId]) return s.legacyTabId;
    retargetLegacy(s.connId ?? useConnectionsStore.getState().selectedId ?? '');
    return LEGACY_DOC_ID;
  };

  const legacy = {
    setConnId: (connId: string | null) => {
      const s = get();
      if (!connId) return;
      const bound = s.legacyTabId ? s.docs[s.legacyTabId] : undefined;
      if (bound?.connId === connId) return;
      // A workspace tab's connection is fixed; only the private doc follows the tree.
      if (bound && s.legacyTabId !== LEGACY_DOC_ID) return;
      retargetLegacy(connId);
    },
    applyConnectionDefaults: (conn: Connection | undefined) => {
      const id = legacyTarget();
      const settings = useSettingsStore.getState().settings;
      edit(id, (d) => ({
        session: conn ? { ...conn.session } : {},
        output: {
          ...d.output,
          dir: conn?.outputDir ?? settings?.defaultOutputDir ?? d.output.dir,
          csv: {
            ...(d.output.csv ?? defaultCsvOpts()),
            crlf: settings?.csvCrlfDefault ?? d.output.csv?.crlf ?? isWindows(),
            bom: d.output.csv?.bom ?? isWindows(),
          },
        },
      }));
    },
    effectiveConnId: () => get().connId ?? useConnectionsStore.getState().selectedId,
    loadFromHistory: (entry: HistoryEntry) => {
      useConnectionsStore.getState().select(entry.connId);
      const init: QueryDocInit = {
        sql: entry.sql,
        sqlMode: entry.sqlFile ? 'file' : 'text',
        sqlFile: entry.sqlFile ?? '',
        name: entry.label,
        session: { ...entry.session },
        output: { ...defaultOutputOpts(), ...entry.output },
      };
      const s = get();
      const bound = s.legacyTabId ? s.docs[s.legacyTabId] : undefined;
      if (bound && bound.connId === entry.connId && s.legacyTabId !== LEGACY_DOC_ID) {
        const id = s.legacyTabId as string;
        edit(id, () => ({
          sqlMode: init.sqlMode,
          sql: entry.sql,
          sqlFile: init.sqlFile,
          sqlFileInfo: null,
          name: entry.label,
          session: { ...entry.session },
          output: normalizeOutput({ ...defaultOutputOpts(), ...entry.output }),
          lastPreflight: null,
          lastPlan: null,
          lastCheck: null,
          error: null,
        }));
      } else {
        retargetLegacy(entry.connId, init);
      }
      useUiStore.getState().setCenterTab('extract');
    },
  };

  const isStr = (v: unknown): v is string => typeof v === 'string';

  return {
    docs: {},
    preflightDialog: null,
    preflightQueue: [],
    legacyTabId: null,
    ...mirrorOf(undefined, null),

    createDoc: api.createDoc,
    disposeDoc: api.disposeDoc,
    restoreDoc: api.restoreDoc,
    startAcknowledged: api.startAcknowledged,
    openResumeDialog: api.openResumeDialog,
    closeDialog: api.closeDialog,
    setPanel: api.setPanel,
    togglePanel: api.togglePanel,
    setCsvEncoding: api.setCsvEncoding,

    // Overloads: the per-tab form takes `tabId` first; the legacy form does not.
    setSqlMode: ((...a: string[]) =>
      a.length >= 2 ? api.setSqlMode(a[0], a[1] as SqlMode) : api.setSqlMode(legacyTarget(), a[0] as SqlMode)) as ExtractState['setSqlMode'],
    setSql: ((...a: string[]) => (a.length >= 2 ? api.setSql(a[0], a[1]) : api.setSql(legacyTarget(), a[0]))) as ExtractState['setSql'],
    setSqlFile: ((...a: string[]) =>
      a.length >= 2 ? api.setSqlFile(a[0], a[1]) : api.setSqlFile(legacyTarget(), a[0])) as ExtractState['setSqlFile'],
    setName: ((...a: string[]) => (a.length >= 2 ? api.setName(a[0], a[1]) : api.setName(legacyTarget(), a[0]))) as ExtractState['setName'],
    setSession: ((a: unknown, b?: Partial<SessionOpts>) =>
      isStr(a) ? api.setSession(a, b ?? {}) : api.setSession(legacyTarget(), a as Partial<SessionOpts>)) as ExtractState['setSession'],
    setOutput: ((a: unknown, b?: Partial<OutputOpts>) =>
      isStr(a) ? api.setOutput(a, b ?? {}) : api.setOutput(legacyTarget(), a as Partial<OutputOpts>)) as ExtractState['setOutput'],
    setCsv: ((a: unknown, b?: Partial<CsvOpts>) =>
      isStr(a) ? api.setCsv(a, b ?? {}) : api.setCsv(legacyTarget(), a as Partial<CsvOpts>)) as ExtractState['setCsv'],
    setParquet: ((a: unknown, b?: Partial<ParquetOpts>) =>
      isStr(a) ? api.setParquet(a, b ?? {}) : api.setParquet(legacyTarget(), a as Partial<ParquetOpts>)) as ExtractState['setParquet'],
    setSplit: ((a: unknown, b?: SplitOpts) =>
      isStr(a) ? api.setSplit(a, b) : api.setSplit(legacyTarget(), a as SplitOpts | undefined)) as ExtractState['setSplit'],
    applyExcelPreset: ((a?: unknown) => api.applyExcelPreset(isStr(a) ? a : legacyTarget())) as ExtractState['applyExcelPreset'],
    clearError: ((a?: unknown) => api.clearError(isStr(a) ? a : legacyTarget())) as ExtractState['clearError'],
    buildRequest: ((a?: unknown) => api.buildRequest(isStr(a) ? a : legacyTarget())) as ExtractState['buildRequest'],
    check: ((a?: unknown) => api.check(isStr(a) ? a : legacyTarget())) as ExtractState['check'],
    checkSql: ((...a: string[]) =>
      a.length >= 2 ? api.checkSql(a[0], a[1]) : api.checkSql(legacyTarget(), a[0])) as ExtractState['checkSql'],
    run: ((a?: unknown) => api.run(isStr(a) ? a : legacyTarget())) as ExtractState['run'],
    explain: ((a?: unknown, b?: boolean) =>
      isStr(a) ? api.explain(a, b) : api.explain(legacyTarget(), a === true)) as ExtractState['explain'],
    loadSqlFile: ((a?: unknown, b?: string) =>
      isStr(a) && (b !== undefined || a in get().docs)
        ? api.loadSqlFile(a, b)
        : api.loadSqlFile(legacyTarget(), isStr(a) ? a : undefined)) as ExtractState['loadSqlFile'],

    bindLegacyDoc: (tabId) => set({ legacyTabId: tabId }),
    setConnId: legacy.setConnId,
    applyConnectionDefaults: legacy.applyConnectionDefaults,
    effectiveConnId: legacy.effectiveConnId,
    loadFromHistory: legacy.loadFromHistory,
  };
});

// Keep the legacy flat fields in step with the bound doc and the sheet (removed in W3).
useExtractStore.subscribe((s, prev) => {
  const doc = s.legacyTabId ? s.docs[s.legacyTabId] : undefined;
  const prevDoc = prev.legacyTabId ? prev.docs[prev.legacyTabId] : undefined;
  if (s.legacyTabId === prev.legacyTabId && doc === prevDoc && s.preflightDialog === prev.preflightDialog) return;
  if (!s.legacyTabId) {
    // Unbound: the flat fields keep whatever they hold; only the sheet state follows.
    if (s.preflightDialog !== prev.preflightDialog) {
      useExtractStore.setState({ busy: s.preflightDialog?.busy ? 'starting' : null, error: s.preflightDialog?.error ?? null });
    }
    return;
  }
  useExtractStore.setState(mirrorOf(doc, s.preflightDialog));
});

/** Select from one doc (shallow-compared, so objects are fine); `undefined` when it is gone. */
export function useQueryDoc<T>(tabId: string, select: (d: QueryDoc) => T): T | undefined {
  return useExtractStore(useShallow((s: ExtractState) => {
    const d = s.docs[tabId];
    return d ? select(d) : undefined;
  }));
}
