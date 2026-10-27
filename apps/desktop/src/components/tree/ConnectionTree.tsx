'use client';

// Connections -> databases -> schemas -> tables. Row counts are always labelled "estimated".
// Expanding a node only flips `expanded` in the catalog store; an effect per node loads its
// children while it is expanded and not cached, so a refresh repopulates open subtrees.

import type { Connection, DatabaseInfo, SchemaInfo, TableInfo, TableKind } from 'ipc-types';
import {
  ChevronDown,
  ChevronRight,
  Columns3,
  Copy,
  Database,
  FileCode2,
  FileInput,
  FolderTree,
  Info,
  Layers,
  MoreHorizontal,
  Pencil,
  Plug,
  Plus,
  RefreshCw,
  ScanSearch,
  Table2,
  Trash2,
  View,
} from 'lucide-react';
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { catalogColumns, catalogSelectSql } from '@/lib/commands';
import { pickAnyFile } from '@/lib/dialogs';
import { formatBytes, formatCompact } from '@/lib/format';
import { t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { cn, copyText } from '@/lib/utils';
import { ImportDbeaverDialog, type ImportDbeaverSource } from '@/components/dialogs/ImportDbeaverDialog';
import { StatusDot } from '@/components/layout/HeaderBar';
import { Badge, ContextMenu, DropdownMenu, EmptyState, IconButton, Spinner, Tooltip, type MenuItem } from '@/components/ui';
import { dbKey, schemaKey, useCatalogStore } from '@/stores/catalog';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

interface TreeRowProps {
  depth: number;
  label: ReactNode;
  icon?: ReactNode;
  expandable?: boolean;
  expanded?: boolean;
  loading?: boolean;
  selected?: boolean;
  onClick?: () => void;
  trailing?: ReactNode;
  title?: string;
}

function TreeRow({ depth, label, icon, expandable, expanded, loading, selected, onClick, trailing, title }: TreeRowProps) {
  return (
    <div
      role="treeitem"
      aria-expanded={expandable ? !!expanded : undefined}
      aria-selected={selected || undefined}
      title={title}
      onClick={onClick}
      className={cn(
        'flex h-7 cursor-pointer select-none items-center gap-1 pr-2 text-[13px] hover:bg-muted',
        selected && 'bg-accent/10 text-accent',
      )}
      style={{ paddingLeft: depth * 14 + 4 }}
    >
      <span className="inline-flex w-4 shrink-0 items-center justify-center text-muted-foreground">
        {loading ? <Spinner size={11} /> : expandable ? expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} /> : null}
      </span>
      {icon && <span className="inline-flex w-4 shrink-0 items-center justify-center text-muted-foreground">{icon}</span>}
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {trailing && <span className="ml-1 flex shrink-0 items-center gap-1">{trailing}</span>}
    </div>
  );
}

const kindIcon: Record<TableKind, ReactNode> = {
  table: <Table2 size={13} />,
  view: <View size={13} />,
  materialized_view: <View size={13} className="text-accent" />,
  partitioned: <Layers size={13} />,
  foreign: <Plug size={13} />,
  other: <Table2 size={13} />,
};

/** `≈ 48.2M` + an explicit "estimated" tag. Row counts are statistics until the user counts. */
export function EstimatedRows({ rows }: { rows: TableInfo['estimatedRows'] }) {
  if (rows === undefined || rows === null) return null;
  return (
    <Tooltip content={`${t('table.estimatedRows')}: ${formatCompact(rows)} (${t('estimated')})`}>
      <Badge tone="neutral" aria-label={`estimated rows ${formatCompact(rows)}`}>
        <span className="tabular-nums">{'≈'}{formatCompact(rows)}</span>
        <span className="text-[10px] uppercase opacity-70">{t('estimated')}</span>
      </Badge>
    </Tooltip>
  );
}

/**
 * Load a node's children while it is expanded and nothing is cached, loading or failed. The
 * store dedupes concurrent loads of the same key, so StrictMode double effects are harmless.
 */
function useLoadWhenExpanded(expanded: boolean, missing: boolean, key: string, load: () => Promise<unknown>, blocked = false) {
  useEffect(() => {
    if (!expanded || !missing || blocked) return;
    const s = useCatalogStore.getState();
    if (s.loading[key] || s.errors[key]) return;
    void load().catch(() => undefined);
  }, [expanded, missing, key, load, blocked]);
}

function TableNode({ connId, db, table, depth }: { connId: string; db: string; table: TableInfo; depth: number }) {
  const selected = useUiStore(
    (s) =>
      s.selectedTable?.connId === connId &&
      s.selectedTable.ref.database === db &&
      s.selectedTable.ref.name === table.name &&
      s.selectedTable.ref.schema === table.schema,
  );
  const selectTable = useUiStore((s) => s.selectTable);
  const ref = { database: db, schema: table.schema, name: table.name };

  const makeSelect = async () => {
    try {
      const sql = await catalogSelectSql(connId, ref);
      const extract = useExtractStore.getState();
      useConnectionsStore.getState().select(connId);
      extract.setConnId(connId);
      extract.setSqlMode('text');
      extract.setSql(sql);
      extract.setName(table.name);
      useUiStore.getState().setCenterTab('extract');
    } catch (e) {
      logLocal('error', `catalog_select_sql: ${errorText(e)}`);
    }
  };
  const copyColumns = async () => {
    try {
      const cols = await catalogColumns(connId, ref);
      await copyText(cols.map((c) => c.name).join(', '));
      logLocal('info', `${t('common.copied')}: ${cols.length} columns of ${table.schema}.${table.name}`);
    } catch (e) {
      logLocal('error', `catalog_columns: ${errorText(e)}`);
    }
  };
  const items: MenuItem[] = [
    { id: 'select', label: t('tree.makeSelect'), icon: <FileCode2 size={13} />, onSelect: () => void makeSelect() },
    { id: 'copy', label: t('tree.copyColumns'), icon: <Copy size={13} />, onSelect: () => void copyColumns() },
    { id: 'open', label: t('tree.openTable'), icon: <Info size={13} />, onSelect: () => selectTable({ connId, ref }), separatorBefore: true },
  ];
  return (
    <ContextMenu items={items}>
      <TreeRow
        depth={depth}
        icon={kindIcon[table.kind]}
        label={table.name}
        selected={selected}
        title={table.comment ?? `${table.schema}.${table.name}`}
        onClick={() => selectTable({ connId, ref })}
        trailing={
          <>
            <EstimatedRows rows={table.estimatedRows} />
            {table.totalBytes !== undefined && (
              <span className="text-[11px] tabular-nums text-muted-foreground">{formatBytes(table.totalBytes, 0)}</span>
            )}
          </>
        }
      />
    </ContextMenu>
  );
}

function SchemaNode({ connId, db, schema, depth }: { connId: string; db: string; schema: SchemaInfo; depth: number }) {
  const key = schemaKey(connId, db, schema.name);
  const expanded = useCatalogStore((s) => !!s.expanded[key]);
  const loading = useCatalogStore((s) => !!s.loading[key]);
  const error = useCatalogStore((s) => s.errors[key]);
  const tables = useCatalogStore((s) => s.tables[key]);
  const toggle = useCatalogStore((s) => s.toggle);
  const loadTables = useCatalogStore((s) => s.loadTables);
  const load = useCallback(() => loadTables(connId, db, schema.name, !!error), [loadTables, connId, db, schema.name, error]);
  useLoadWhenExpanded(expanded, !tables, key, load);

  const onClick = () => {
    toggle(key);
    // Re-opening a node that failed retries it.
    if (!expanded && error) void load().catch(() => undefined);
  };
  return (
    <>
      <TreeRow
        depth={depth}
        icon={<FolderTree size={13} />}
        label={schema.name}
        expandable
        expanded={expanded}
        loading={loading}
        onClick={onClick}
        trailing={schema.tableCount !== undefined ? <span className="text-[11px] text-muted-foreground">{String(schema.tableCount)}</span> : undefined}
      />
      {expanded && error && <div className="px-6 py-1 text-xs text-danger">{error}</div>}
      {expanded && tables && tables.length === 0 && <div className="px-6 py-1 text-xs text-muted-foreground">{t('tree.noTables')}</div>}
      {expanded && tables?.map((tb) => <TableNode key={tb.name} connId={connId} db={db} table={tb} depth={depth + 1} />)}
    </>
  );
}

function DatabaseNode({ connId, database, depth }: { connId: string; database: DatabaseInfo; depth: number }) {
  const key = dbKey(connId, database.name);
  const expanded = useCatalogStore((s) => !!s.expanded[key]);
  const loading = useCatalogStore((s) => !!s.loading[key]);
  const error = useCatalogStore((s) => s.errors[key]);
  const schemas = useCatalogStore((s) => s.schemas[key]);
  const toggle = useCatalogStore((s) => s.toggle);
  const loadSchemas = useCatalogStore((s) => s.loadSchemas);
  const load = useCallback(() => loadSchemas(connId, database.name, !!error), [loadSchemas, connId, database.name, error]);
  useLoadWhenExpanded(expanded && database.accessible, !schemas, key, load);

  const onClick = () => {
    if (!database.accessible) return;
    toggle(key);
    if (!expanded && error) void load().catch(() => undefined);
  };
  return (
    <>
      <TreeRow
        depth={depth}
        icon={<Database size={13} />}
        label={
          <span className={cn(!database.accessible && 'text-muted-foreground line-through')}>
            {database.name}
            {database.isCurrent && <span className="ml-1 text-[10px] text-muted-foreground">(current)</span>}
          </span>
        }
        expandable={database.accessible}
        expanded={expanded}
        loading={loading}
        onClick={onClick}
        title={!database.accessible ? t('tree.inaccessible') : database.encoding ? `${database.encoding} ${database.collation ?? ''}` : undefined}
        trailing={database.sizeBytes !== undefined ? <span className="text-[11px] tabular-nums text-muted-foreground">{formatBytes(database.sizeBytes, 0)}</span> : undefined}
      />
      {expanded && error && <div className="px-6 py-1 text-xs text-danger">{error}</div>}
      {expanded && schemas && schemas.length === 0 && <div className="px-6 py-1 text-xs text-muted-foreground">{t('tree.noSchemas')}</div>}
      {expanded && schemas?.map((sc) => <SchemaNode key={sc.name} connId={connId} db={database.name} schema={sc} depth={depth + 1} />)}
    </>
  );
}

function ConnectionNode({ conn }: { conn: Connection }) {
  const key = conn.id;
  const expanded = useCatalogStore((s) => !!s.expanded[key]);
  const loading = useCatalogStore((s) => !!s.loading[key]);
  const error = useCatalogStore((s) => s.errors[key]);
  const databases = useCatalogStore((s) => s.databases[key]);
  const toggle = useCatalogStore((s) => s.toggle);
  const loadDatabases = useCatalogStore((s) => s.loadDatabases);
  const refresh = useCatalogStore((s) => s.refresh);
  const status = useConnectionsStore((s) => s.status[conn.id] ?? 'unknown');
  const statusError = useConnectionsStore((s) => s.errors[conn.id]);
  const selected = useConnectionsStore((s) => s.selectedId === conn.id);
  const select = useConnectionsStore((s) => s.select);
  const test = useConnectionsStore((s) => s.test);
  const remove = useConnectionsStore((s) => s.remove);
  const needsPassword = useConnectionsStore((s) => needsSessionPassword(s, conn.id));
  const openConnectionDialog = useUiStore((s) => s.openConnectionDialog);
  const askDbPassword = useUiStore((s) => s.askDbPassword);
  const load = useCallback(() => loadDatabases(conn.id, !!error), [loadDatabases, conn.id, error]);
  useLoadWhenExpanded(expanded, !databases, key, load, needsPassword);

  const onClick = () => {
    select(conn.id);
    useExtractStore.getState().setConnId(conn.id);
    if (!expanded && needsPassword) {
      // "Ask every time": the password dialog expands the node once the password is accepted.
      askDbPassword(conn.id, 'browse');
      return;
    }
    toggle(key);
    if (!expanded && error) void load().catch(() => undefined);
  };
  const onTest = async () => {
    if (needsPassword) {
      askDbPassword(conn.id, 'test');
      return;
    }
    try {
      await test(conn.id);
      logLocal('info', `${conn.name}: ${t('tree.status.ok')}`);
    } catch (e) {
      logLocal('error', `${conn.name}: ${errorText(e)}`);
    }
  };
  const onDelete = async () => {
    if (!window.confirm(t('tree.deleteConfirm', { name: conn.name }))) return;
    try {
      await remove(conn.id);
      useCatalogStore.getState().clearConnection(conn.id);
    } catch (e) {
      logLocal('error', `conn_delete: ${errorText(e)}`);
    }
  };
  const items: MenuItem[] = [
    { id: 'test', label: t('tree.test'), icon: <Plug size={13} />, onSelect: () => void onTest() },
    { id: 'refresh', label: t('tree.refresh'), icon: <RefreshCw size={13} />, onSelect: () => void refresh(conn.id).catch(() => undefined) },
    { id: 'edit', label: t('tree.edit'), icon: <Pencil size={13} />, onSelect: () => openConnectionDialog(conn.id), separatorBefore: true },
    { id: 'delete', label: t('tree.delete'), icon: <Trash2 size={13} />, onSelect: () => void onDelete(), danger: true },
  ];
  return (
    <>
      <ContextMenu items={items}>
        <TreeRow
          depth={0}
          icon={<StatusDot status={status} title={statusError} />}
          label={
            <span className="flex items-center gap-1.5">
              <span className={cn('truncate', selected && 'font-medium')}>{conn.name}</span>
              <span className="text-[11px] text-muted-foreground">{conn.driver}</span>
              {conn.ssh?.enabled && (
                <Badge tone="outline" title={`${conn.ssh.user}@${conn.ssh.host}:${conn.ssh.port}`}>
                  ssh
                </Badge>
              )}
            </span>
          }
          expandable
          expanded={expanded}
          loading={loading || status === 'testing'}
          selected={selected}
          onClick={onClick}
        />
      </ContextMenu>
      {expanded && error && <div className="px-6 py-1 text-xs text-danger">{error}</div>}
      {expanded && databases && databases.length === 0 && <div className="px-6 py-1 text-xs text-muted-foreground">{t('tree.noDatabases')}</div>}
      {expanded && databases?.map((db) => <DatabaseNode key={db.name} connId={conn.id} database={db} depth={1} />)}
    </>
  );
}

export function ConnectionTree() {
  const connections = useConnectionsStore((s) => s.connections);
  const loaded = useConnectionsStore((s) => s.loaded);
  const openConnectionDialog = useUiStore((s) => s.openConnectionDialog);
  const selectedId = useConnectionsStore((s) => s.selectedId);
  const refresh = useCatalogStore((s) => s.refresh);
  const rescanDrivers = useConnectionsStore((s) => s.rescanDrivers);
  const importDbeaver = useConnectionsStore((s) => s.importDbeaver);
  const [importSource, setImportSource] = useState<ImportDbeaverSource | null>(null);

  const onRefresh = useCallback(() => {
    if (selectedId) void refresh(selectedId).catch(() => undefined);
  }, [selectedId, refresh]);

  const onImport = async () => {
    let path: string | null;
    try {
      path = await pickAnyFile([{ name: 'DBeaver data-sources.json', extensions: ['json'] }]);
    } catch (e) {
      logLocal('error', `file dialog: ${errorText(e)}`);
      return;
    }
    if (!path) return;
    try {
      setImportSource({ path, rows: await importDbeaver(path) });
    } catch (e) {
      logLocal('error', `conn_import_dbeaver: ${errorText(e)}`);
      setImportSource({ path, rows: [], error: errorText(e) });
    }
  };

  const onRescan = async () => {
    try {
      await rescanDrivers();
      const available = useConnectionsStore.getState().drivers.filter((d) => d.available).length;
      logLocal('info', t('drivers.rescanned', { count: available }));
    } catch (e) {
      logLocal('error', `drivers_rescan: ${errorText(e)}`);
    }
  };

  const menu: MenuItem[] = [
    { id: 'import-dbeaver', label: t('tree.importDbeaver'), icon: <FileInput size={13} />, onSelect: () => void onImport() },
    { id: 'rescan-drivers', label: t('drivers.rescan'), icon: <ScanSearch size={13} />, onSelect: () => void onRescan() },
  ];

  return (
    <section className="flex h-full flex-col bg-background" aria-label={t('tree.title')}>
      <header className="flex h-7 shrink-0 items-center gap-1 border-b border-border bg-muted pl-2 pr-1">
        <Columns3 size={13} className="text-muted-foreground" />
        <span className="text-xs font-medium text-muted-foreground">{t('tree.title')}</span>
        <span className="ml-auto flex items-center">
          <IconButton label={t('tree.refresh')} size="sm" onClick={onRefresh} disabled={!selectedId}>
            <RefreshCw size={12} />
          </IconButton>
          <IconButton label={t('tree.add')} size="sm" onClick={() => openConnectionDialog(null)}>
            <Plus size={13} />
          </IconButton>
          <DropdownMenu items={menu}>
            <IconButton label={t('tree.more')} size="sm">
              <MoreHorizontal size={13} />
            </IconButton>
          </DropdownMenu>
        </span>
      </header>
      {importSource && <ImportDbeaverDialog source={importSource} onClose={() => setImportSource(null)} />}
      <div role="tree" className="min-h-0 flex-1 overflow-auto py-1">
        {loaded && connections.length === 0 ? (
          <EmptyState title={t('tree.empty')} action={<button className="text-accent underline-offset-2 hover:underline" onClick={() => openConnectionDialog(null)}>{t('tree.add')}</button>} />
        ) : (
          connections.map((c) => <ConnectionNode key={c.id} conn={c} />)
        )}
      </div>
    </section>
  );
}
