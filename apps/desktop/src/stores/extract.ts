'use client';

// Extract form state (SQL, session and output options), the preflight/start/explain flow and
// the preflight dialog. Job state after `export_start` is owned by the jobs store.

import type {
  Connection,
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
} from 'ipc-types';
import { create } from 'zustand';
import { exportPreflight, exportStart, queryExplain, sqlCheck, sqlFileRead } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { t } from '@/lib/i18n';
import { isWindows } from '@/lib/platform';
import { needsSessionPassword, useConnectionsStore } from './connections';
import { useJobsStore } from './jobs';
import { useSettingsStore } from './settings';
import { useUiStore } from './ui';

export type SqlMode = 'text' | 'file';
export type ExtractBusy = 'check' | 'preflight' | 'starting' | 'explain' | 'file' | null;

export type PreflightDialogState =
  | { kind: 'run'; req: ExportRequest; report: PreflightReport }
  | { kind: 'resume'; jobId: string; report: PreflightReport };

export const DEFAULT_NAME_PATTERN = '{conn}_{name}_{yyyyMMdd_HHmm}';
/** Excel's sheet limit is 1,048,576 rows; split below it. */
export const EXCEL_SPLIT_ROWS = 1_000_000;

export function defaultCsvOpts(crlf = isWindows()): CsvOpts {
  return { header: true, delimiter: ',', nullAs: '', bom: false, crlf };
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

export interface ExtractState {
  /** Target connection; `null` follows the tree selection. */
  connId: string | null;
  sqlMode: SqlMode;
  sql: string;
  sqlFile: string;
  sqlFileInfo: SqlFile | null;
  name: string;
  session: SessionOpts;
  output: OutputOpts;
  lastPreflight: PreflightReport | null;
  lastPlan: PlanSummary | null;
  /** Last SQL guard result; it was computed with the dialect of the connection at that time. */
  lastCheck: SqlCheck | null;
  busy: ExtractBusy;
  error: string | null;
  preflightDialog: PreflightDialogState | null;
  lastJobId: string | null;

  setConnId(connId: string | null): void;
  setSqlMode(mode: SqlMode): void;
  setSql(sql: string): void;
  setSqlFile(path: string): void;
  setName(name: string): void;
  setSession(patch: Partial<SessionOpts>): void;
  setOutput(patch: Partial<OutputOpts>): void;
  setCsv(patch: Partial<CsvOpts>): void;
  setParquet(patch: Partial<ParquetOpts>): void;
  setSplit(split: SplitOpts | undefined): void;
  applyExcelPreset(): void;
  applyConnectionDefaults(conn: Connection | undefined): void;
  clearError(): void;

  effectiveConnId(): string | null;
  buildRequest(): ExportRequest | null;
  /** Run the SQL guard on the current text with the target connection's SQL dialect. */
  check(): Promise<SqlCheck | null>;
  /**
   * Run the SQL guard on `sql` (e.g. a live check while typing) with the dialect of the target
   * connection (`effectiveConnId()`), without touching `busy` / `lastCheck`. `null` when the
   * check itself failed (the error is not surfaced: the explicit check / run will report it).
   */
  checkSql(sql: string): Promise<SqlCheck | null>;
  /** Preflight, then either start immediately or open the dialog for warnings/blockers. */
  run(): Promise<void>;
  /** Start (or resume) with the given acknowledged warning codes; closes the dialog. */
  startAcknowledged(acks: string[]): Promise<string | null>;
  explain(analyze?: boolean): Promise<void>;
  loadSqlFile(path?: string): Promise<void>;
  loadFromHistory(entry: HistoryEntry): void;
  openResumeDialog(jobId: string, report: PreflightReport): void;
  closeDialog(): void;
}

export const useExtractStore = create<ExtractState>()((set, get) => ({
  connId: null,
  sqlMode: 'text',
  sql: '',
  sqlFile: '',
  sqlFileInfo: null,
  name: '',
  session: {},
  output: defaultOutputOpts(),
  lastPreflight: null,
  lastPlan: null,
  lastCheck: null,
  busy: null,
  error: null,
  preflightDialog: null,
  lastJobId: null,

  setConnId: (connId) => {
    if (get().connId === connId) return;
    // The last check was made with the old connection's SQL dialect.
    set({ connId, lastPreflight: null, lastPlan: null, lastCheck: null });
    const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId);
    get().applyConnectionDefaults(conn);
  },
  // A plan or check belongs to the exact text it was made for: drop both when the text changes,
  // so an old EXPLAIN is never shown under a different query.
  setSqlMode: (mode) => set((s) => (s.sqlMode === mode ? {} : { sqlMode: mode, lastCheck: null, lastPlan: null })),
  setSql: (sql) => set((s) => (s.sql === sql ? {} : { sql, lastCheck: null, lastPlan: null, error: null })),
  setSqlFile: (path) => set((s) => (s.sqlFile === path ? {} : { sqlFile: path, sqlFileInfo: null, lastCheck: null, lastPlan: null })),
  setName: (name) => set({ name }),
  setSession: (patch) => set((s) => ({ session: { ...s.session, ...patch } })),
  setOutput: (patch) => set((s) => ({ output: { ...s.output, ...patch } })),
  setCsv: (patch) => set((s) => ({ output: { ...s.output, csv: { ...(s.output.csv ?? defaultCsvOpts()), ...patch } } })),
  setParquet: (patch) =>
    set((s) => ({ output: { ...s.output, parquet: { ...(s.output.parquet ?? defaultParquetOpts()), ...patch } } })),
  setSplit: (split) => set((s) => ({ output: { ...s.output, split } })),

  applyExcelPreset: () =>
    set((s) => ({
      output: {
        ...s.output,
        format: 'csv',
        csv: { header: true, delimiter: ',', nullAs: '', bom: true, crlf: true },
        compress: undefined,
        split: { rows: BigInt(EXCEL_SPLIT_ROWS) },
      },
    })),

  applyConnectionDefaults: (conn) => {
    const settings = useSettingsStore.getState().settings;
    set((s) => ({
      session: conn ? { ...conn.session } : {},
      output: {
        ...s.output,
        dir: conn?.outputDir ?? settings?.defaultOutputDir ?? s.output.dir,
        csv: { ...(s.output.csv ?? defaultCsvOpts()), crlf: settings?.csvCrlfDefault ?? s.output.csv?.crlf ?? isWindows() },
      },
    }));
  },

  clearError: () => set({ error: null }),

  effectiveConnId: () => get().connId ?? useConnectionsStore.getState().selectedId,

  buildRequest: () => {
    const s = get();
    const connId = s.effectiveConnId();
    if (!connId) {
      set({ error: t('extract.noConnection') });
      return null;
    }
    const sql = s.sqlMode === 'text' ? s.sql.trim() : '';
    const sqlFile = s.sqlMode === 'file' ? s.sqlFile.trim() : '';
    if (!sql && !sqlFile) {
      set({ error: t('extract.emptySql') });
      return null;
    }
    const output: OutputOpts = {
      ...s.output,
      csv: s.output.format === 'csv' ? (s.output.csv ?? defaultCsvOpts()) : undefined,
      parquet: s.output.format === 'parquet' ? (s.output.parquet ?? defaultParquetOpts()) : undefined,
      compress: s.output.format === 'csv' ? s.output.compress : undefined,
    };
    return {
      connId,
      sql: sql || undefined,
      sqlFile: sqlFile || undefined,
      name: s.name.trim() || undefined,
      session: { ...s.session },
      output,
    };
  },

  check: async () => {
    const s = get();
    const sql = s.sqlMode === 'text' ? s.sql : (s.sqlFileInfo?.content ?? '');
    const connId = s.effectiveConnId();
    set({ busy: 'check', error: null });
    try {
      const result = await sqlCheck(sql, connId);
      // Drop a result that no longer matches the target connection (it changed meanwhile).
      if (get().effectiveConnId() === connId) set({ lastCheck: result });
      return result;
    } catch (e) {
      set({ error: errorText(e) });
      return null;
    } finally {
      set({ busy: null });
    }
  },

  checkSql: async (sql) => {
    try {
      return await sqlCheck(sql, get().effectiveConnId());
    } catch {
      return null;
    }
  },

  run: async () => {
    if (get().busy) return;
    const req = get().buildRequest();
    if (!req) return;
    if (needsSessionPassword(useConnectionsStore.getState(), req.connId)) {
      // The password dialog runs `run()` again once the backend accepted the password.
      useUiStore.getState().askDbPassword(req.connId, 'run');
      return;
    }
    set({ busy: 'preflight', error: null });
    try {
      const report = await exportPreflight(req);
      set({ lastPreflight: report });
      if (report.blockers.length > 0 || report.warnings.length > 0) {
        set({ preflightDialog: { kind: 'run', req, report } });
        return;
      }
      set({ busy: 'starting' });
      const jobId = await exportStart(req, []);
      set({ lastJobId: jobId });
    } catch (e) {
      set({ error: errorText(e) });
    } finally {
      set({ busy: null });
    }
  },

  startAcknowledged: async (acks) => {
    const dialog = get().preflightDialog;
    set({ busy: 'starting', error: null });
    try {
      if (dialog?.kind === 'resume') {
        await useJobsStore.getState().resume(dialog.jobId, acks);
        set({ preflightDialog: null });
        return dialog.jobId;
      }
      const req = dialog?.kind === 'run' ? dialog.req : get().buildRequest();
      if (!req) return null;
      const jobId = await exportStart(req, acks);
      set({ lastJobId: jobId, preflightDialog: null });
      return jobId;
    } catch (e) {
      set({ error: errorText(e) });
      return null;
    } finally {
      set({ busy: null });
    }
  },

  explain: async (analyze = false) => {
    if (get().busy) return;
    const s = get();
    const connId = s.effectiveConnId();
    if (!connId) {
      set({ error: t('extract.noConnection') });
      return;
    }
    const sql = s.sqlMode === 'text' ? s.sql.trim() : (s.sqlFileInfo?.content.trim() ?? '');
    if (!sql) {
      set({ error: t('extract.emptySql') });
      return;
    }
    if (needsSessionPassword(useConnectionsStore.getState(), connId)) {
      useUiStore.getState().askDbPassword(connId, 'explain');
      return;
    }
    set({ busy: 'explain', error: null });
    try {
      const plan = await queryExplain(connId, sql, analyze);
      set({ lastPlan: plan });
    } catch (e) {
      set({ error: errorText(e) });
    } finally {
      set({ busy: null });
    }
  },

  loadSqlFile: async (path) => {
    const p = (path ?? get().sqlFile).trim();
    if (!p) return;
    set({ busy: 'file', error: null, sqlFile: p });
    try {
      const info = await sqlFileRead(p);
      set({ sqlFileInfo: info, sqlMode: 'file', lastCheck: null });
    } catch (e) {
      set({ error: errorText(e), sqlFileInfo: null });
    } finally {
      set({ busy: null });
    }
  },

  loadFromHistory: (entry) => {
    useConnectionsStore.getState().select(entry.connId);
    set({
      connId: entry.connId,
      sqlMode: entry.sqlFile ? 'file' : 'text',
      sql: entry.sql,
      sqlFile: entry.sqlFile ?? '',
      sqlFileInfo: null,
      name: entry.label,
      session: { ...entry.session },
      output: { ...defaultOutputOpts(), ...entry.output },
      lastPreflight: null,
      lastPlan: null,
      lastCheck: null,
      error: null,
    });
    useUiStore.getState().setCenterTab('extract');
  },

  openResumeDialog: (jobId, report) => set({ preflightDialog: { kind: 'resume', jobId, report } }),
  closeDialog: () => set({ preflightDialog: null }),
}));
