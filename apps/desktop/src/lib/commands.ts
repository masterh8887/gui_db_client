// One typed async function per Tauri command. The command names and argument names are the
// contract with `apps/desktop/src-tauri/src/commands.rs` (see the table at its top); payload
// types come from `ipc-types`. Optional arguments left `undefined` are omitted on the wire.

import type {
  AppInfo,
  ColumnInfo,
  Connection,
  ConnectionInput,
  DatabaseInfo,
  DiagnosticsBundle,
  DriverInfo,
  ExactCount,
  ExportRequest,
  HistoryEntry,
  ImportedConnection,
  IndexInfo,
  JobSummary,
  LogEvent,
  PlanSummary,
  PreflightReport,
  ProbeResult,
  SchemaInfo,
  Settings,
  SqlCheck,
  SqlFile,
  TableInfo,
  TableRef,
} from 'ipc-types';
import { invoke } from './ipc';

// Re-exported for callers that take the command and its result type from one place.
export type { AppInfo, SqlCheck } from 'ipc-types';
/** @deprecated The generated `SqlCheck` from `ipc-types` (same shape). */
export type SqlCheckResult = SqlCheck;

// ---- drivers -----------------------------------------------------------------------------

export const driversList = () => invoke<DriverInfo[]>('drivers_list');
export const driversRescan = () => invoke<DriverInfo[]>('drivers_rescan');

// ---- connections ---------------------------------------------------------------------------

export const connList = () => invoke<Connection[]>('conn_list');
/** One saved connection (with `hasPassword` / `hasSshSecret` freshly read from the keyring). */
export const connGet = (connId: string) => invoke<Connection>('conn_get', { connId });
export const connSave = (input: ConnectionInput) => invoke<Connection>('conn_save', { input });
export const connDelete = (connId: string) => invoke<void>('conn_delete', { connId });
export const connTest = (connId: string, password?: string) =>
  invoke<ProbeResult>('conn_test', { connId, password });
export const connImportDbeaver = (path: string) =>
  invoke<ImportedConnection[]>('conn_import_dbeaver', { path });

// ---- catalog -------------------------------------------------------------------------------

export const catalogDatabases = (connId: string, refresh = false) =>
  invoke<DatabaseInfo[]>('catalog_databases', { connId, refresh });
export const catalogSchemas = (connId: string, db: string, refresh = false) =>
  invoke<SchemaInfo[]>('catalog_schemas', { connId, db, refresh });
export const catalogTables = (connId: string, db: string, schema: string, refresh = false) =>
  invoke<TableInfo[]>('catalog_tables', { connId, db, schema, refresh });
export const catalogColumns = (connId: string, table: TableRef) =>
  invoke<ColumnInfo[]>('catalog_columns', { connId, table });
export const catalogIndexes = (connId: string, table: TableRef) =>
  invoke<IndexInfo[]>('catalog_indexes', { connId, table });
export const catalogDdl = (connId: string, table: TableRef) =>
  invoke<string>('catalog_ddl', { connId, table });
export const catalogSelectSql = (connId: string, table: TableRef) =>
  invoke<string>('catalog_select_sql', { connId, table });
/** Starts an `exact_count` job and returns its id; the result arrives as `job:done.exactCount`. */
export const catalogExactCount = (connId: string, table: TableRef) =>
  invoke<string>('catalog_exact_count', { connId, table });
/**
 * The last exact count that finished for `table` on this connection (kept in the backend's
 * history database, so it survives restarts); `null` when the table was never counted.
 * Reads local state only: it never connects to the server.
 */
export const catalogLastExactCount = (connId: string, table: TableRef) =>
  invoke<ExactCount | null>('catalog_last_exact_count', { connId, table });
/** Drop the backend's cached metadata of one connection (connection-level refresh). */
export const catalogInvalidate = (connId: string) => invoke<void>('catalog_invalidate', { connId });

// ---- query ---------------------------------------------------------------------------------

export const queryExplain = (connId: string, sql: string, analyze = false) =>
  invoke<PlanSummary>('query_explain', { connId, sql, analyze });
/**
 * Run the extraction SQL guard. With `connId` the query is read with that connection's SQL
 * dialect (MySQL quoting and comments differ), exactly as EXPLAIN and preflight will read it;
 * without one the standard rules apply. An unknown `connId` rejects with `NOT_FOUND`.
 */
export const sqlCheck = (sql: string, connId?: string | null) =>
  invoke<SqlCheck>('sql_check', { sql, connId: connId ?? undefined });
export const sqlFileRead = (path: string) => invoke<SqlFile>('sql_file_read', { path });

// ---- export / jobs -------------------------------------------------------------------------

export const exportPreflight = (req: ExportRequest) =>
  invoke<PreflightReport>('export_preflight', { req });
/** Returns the new job id. State changes arrive as `job:*` events. */
export const exportStart = (req: ExportRequest, acknowledgedWarnings: string[] = []) =>
  invoke<string>('export_start', { req, acknowledgedWarnings });
export const jobResume = (jobId: string, acknowledgedWarnings: string[]) =>
  invoke<void>('job_resume', { jobId, acknowledgedWarnings });
export const jobCancel = (jobId: string) => invoke<void>('job_cancel', { jobId });
export const jobList = () => invoke<JobSummary[]>('job_list');
export const jobGet = (jobId: string) => invoke<JobSummary>('job_get', { jobId });

// ---- history / settings --------------------------------------------------------------------

export const historyList = (connId?: string, limit?: number) =>
  invoke<HistoryEntry[]>('history_list', { connId, limit });
export const settingsGet = () => invoke<Settings>('settings_get');
export const settingsSet = (settings: Settings) => invoke<Settings>('settings_set', { settings });

// ---- tunnel prompts ------------------------------------------------------------------------

export const tunnelConfirmHostkey = (promptId: string, accept: boolean) =>
  invoke<void>('tunnel_confirm_hostkey', { promptId, accept });
export const tunnelProvideSecret = (promptId: string, secret?: string) =>
  invoke<void>('tunnel_provide_secret', { promptId, secret });

// ---- diagnostics ---------------------------------------------------------------------------

export const logsRecent = (limit?: number) => invoke<LogEvent[]>('logs_recent', { limit });
export const diagnosticsBundle = (outDir: string) =>
  invoke<DiagnosticsBundle>('diagnostics_bundle', { outDir });
export const appInfo = () => invoke<AppInfo>('app_info');
/** Reveal a file in the OS file manager, or open a directory. */
export const openPath = (path: string) => invoke<void>('open_path', { path });
