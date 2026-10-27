'use client';

// Metadata for the selected table: columns, indexes (with partial predicates), DDL, and the row
// count: the estimate (labelled as such) next to the last exact count, which is either read back
// from the backend's history on selection or delivered by a fresh count job's `job:done`.

import type { ColumnInfo, IndexInfo } from 'ipc-types';
import { Calculator, Copy, FileCode2, Key, RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import { catalogExactCount, catalogSelectSql } from '@/lib/commands';
import { formatBytes, formatDateTime, formatDuration, formatInt } from '@/lib/format';
import { t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { copyText, tableKey, tableLabel } from '@/lib/utils';
import { Badge, Button, EmptyState, IconButton, Notice, Spinner, Table, type Column } from '@/components/ui';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { isTerminalState, useJobsStore } from '@/stores/jobs';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

// Built at render time (not module load) so headers follow the current language.
const columnCols = (): Column<ColumnInfo>[] => [
  { id: 'pos', header: '#', cell: (c) => c.position, width: 36, align: 'right', className: 'tabular-nums text-muted-foreground' },
  {
    id: 'name',
    header: t('table.col.name'),
    cell: (c) => (
      <span className="flex items-center gap-1 font-mono">
        {c.primaryKey && <Key size={11} className="text-warning" aria-label="primary key" />}
        {c.name}
      </span>
    ),
  },
  { id: 'type', header: t('table.col.type'), cell: (c) => <span className="font-mono text-muted-foreground">{c.dataType}</span> },
  { id: 'null', header: t('table.col.nullable'), cell: (c) => (c.nullable ? '' : 'NOT NULL'), className: 'text-xs' },
  { id: 'default', header: t('table.col.default'), cell: (c) => <span className="font-mono text-xs">{c.default ?? ''}</span> },
  {
    id: 'key',
    header: t('table.col.key'),
    cell: (c) => (
      <span className="flex gap-1">
        {c.primaryKey && <Badge tone="warning">PK</Badge>}
        {c.unique && !c.primaryKey && <Badge tone="accent">UQ</Badge>}
        {c.foreignKey && <Badge tone="outline" title={c.foreignKey}>FK</Badge>}
      </span>
    ),
  },
  { id: 'comment', header: t('table.col.comment'), cell: (c) => <span className="text-xs text-muted-foreground">{c.comment ?? ''}</span> },
];

const indexCols = (): Column<IndexInfo>[] => [
  { id: 'name', header: t('table.idx.name'), cell: (i) => <span className="font-mono">{i.name}</span> },
  { id: 'cols', header: t('table.idx.columns'), cell: (i) => <span className="font-mono text-xs">{i.columns.join(', ')}</span> },
  { id: 'kind', header: t('table.idx.kind'), cell: (i) => i.kind ?? '', className: 'text-xs' },
  {
    id: 'flags',
    header: t('table.idx.unique'),
    cell: (i) => (
      <span className="flex gap-1">
        {i.primary && <Badge tone="warning">PK</Badge>}
        {i.unique && !i.primary && <Badge tone="accent">UNIQUE</Badge>}
      </span>
    ),
  },
  { id: 'pred', header: t('table.idx.predicate'), cell: (i) => (i.predicate ? <span className="font-mono text-xs text-warning">WHERE {i.predicate}</span> : '') },
  { id: 'include', header: t('table.idx.include'), cell: (i) => <span className="font-mono text-xs">{i.include.join(', ')}</span> },
  { id: 'size', header: t('table.idx.size'), cell: (i) => formatBytes(i.sizeBytes), align: 'right', className: 'tabular-nums' },
  { id: 'scans', header: t('table.idx.scans'), cell: (i) => formatInt(i.scans), align: 'right', className: 'tabular-nums' },
];

export function TableTab() {
  const selected = useUiStore((s) => s.selectedTable);
  const key = selected ? tableKey(selected.connId, selected.ref) : '';
  const details = useCatalogStore((s) => (key ? s.details[key] : undefined));
  const loading = useCatalogStore((s) => (key ? !!s.loading[key] : false));
  const error = useCatalogStore((s) => (key ? s.errors[key] : undefined));
  const info = useCatalogStore((s) => (selected ? s.findTable(selected.connId, selected.ref) : undefined));
  const exact = useCatalogStore((s) => (key ? s.exactCounts[key] : undefined));
  const counting = useCatalogStore((s) => (key ? s.counting[key] : undefined));
  const loadTableDetails = useCatalogStore((s) => s.loadTableDetails);
  const loadLastExactCount = useCatalogStore((s) => s.loadLastExactCount);
  const setCounting = useCatalogStore((s) => s.setCounting);
  const connName = useConnectionsStore((s) => s.connections.find((c) => c.id === selected?.connId)?.name);
  const [copied, setCopied] = useState(false);
  // A handful of small objects per render; building them here keeps `t()` current.
  const cols = { columns: columnCols(), indexes: indexCols() };

  useEffect(() => {
    if (selected) void loadTableDetails(selected.connId, selected.ref).catch(() => undefined);
  }, [selected, loadTableDetails]);

  // The last count the user ran (possibly in an earlier session) is stored by the backend; show
  // it next to the estimate. A `job:done` that lands meanwhile is newer and wins in the store.
  useEffect(() => {
    if (!selected) return;
    void loadLastExactCount(selected.connId, selected.ref).catch((e: unknown) =>
      logLocal('warn', `catalog_last_exact_count: ${errorText(e)}`),
    );
  }, [selected, loadLastExactCount]);

  if (!selected) return <EmptyState title={t('table.noSelection')} />;
  const { connId, ref } = selected;

  const countExactly = async () => {
    try {
      const jobId = await catalogExactCount(connId, ref);
      // Events and the invoke response travel separately: a small table can finish before the
      // job id arrives here. AppEvents has then already handled `job:done` (and routes the
      // count into the catalog), so a flag set now would never be cleared again.
      const job = useJobsStore.getState().jobs[jobId];
      if (job && isTerminalState(job.summary.state)) return;
      setCounting(connId, ref, jobId);
    } catch (e) {
      logLocal('error', `catalog_exact_count: ${errorText(e)}`);
    }
  };
  const makeSelect = async () => {
    try {
      const sql = await catalogSelectSql(connId, ref);
      const extract = useExtractStore.getState();
      useConnectionsStore.getState().select(connId);
      extract.setConnId(connId);
      extract.setSqlMode('text');
      extract.setSql(sql);
      extract.setName(ref.name);
      useUiStore.getState().setCenterTab('extract');
    } catch (e) {
      logLocal('error', `catalog_select_sql: ${errorText(e)}`);
    }
  };
  const copyDdl = async () => {
    if (!details?.ddl) return;
    setCopied(await copyText(details.ddl));
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="flex h-full min-h-0 flex-col overflow-auto">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-border px-3 py-2">
        <h2 className="text-[14px] font-semibold">
          {tableLabel(ref)}
          {ref.database && <span className="ml-1 text-xs font-normal text-muted-foreground">@ {ref.database}</span>}
        </h2>
        {info && <Badge tone="outline">{info.kind}</Badge>}
        {connName && <span className="text-xs text-muted-foreground">{connName}</span>}
        <div className="ml-auto flex items-center gap-1">
          <Button size="sm" variant="outline" icon={<FileCode2 size={13} />} onClick={() => void makeSelect()}>
            {t('table.makeSelect')}
          </Button>
          <IconButton label={t('tree.refresh')} size="sm" onClick={() => void loadTableDetails(connId, ref, true).catch(() => undefined)}>
            <RefreshCw size={12} />
          </IconButton>
        </div>
      </header>

      <section className="flex flex-wrap items-center gap-x-6 gap-y-1 border-b border-border px-3 py-2 text-[13px]">
        <div>
          <span className="text-xs text-muted-foreground">{t('table.estimatedRows')}: </span>
          <b className="tabular-nums">{formatInt(info?.estimatedRows)}</b>{' '}
          <span className="text-[10px] uppercase text-muted-foreground">{t('estimated')}</span>
        </div>
        <div>
          <span className="text-xs text-muted-foreground">{t('table.exactRows')}: </span>
          {exact ? (
            <>
              <b className="tabular-nums">{formatInt(exact.rows)}</b>{' '}
              <span
                className="text-[10px] uppercase text-muted-foreground"
                title={t('table.countedAt', { at: formatDateTime(exact.at, true), ms: formatDuration(exact.elapsedMs) })}
              >
                {t('table.exactAt', { at: formatDateTime(exact.at) })}
              </span>
            </>
          ) : (
            <span className="text-muted-foreground">{'—'}</span>
          )}
          <Button size="sm" variant="outline" className="ml-2" icon={<Calculator size={13} />} onClick={() => void countExactly()} loading={!!counting} disabled={!!counting}>
            {counting ? t('table.counting') : t('table.countExact')}
          </Button>
        </div>
        {info && (
          <div className="text-xs text-muted-foreground">
            {t('table.size')}: {formatBytes(info.totalBytes)} ({t('table.tableSize')} {formatBytes(info.tableBytes)}, {t('table.indexSize')} {formatBytes(info.indexBytes)})
          </div>
        )}
      </section>

      {error && (
        <div className="p-2">
          <Notice tone="danger">{error}</Notice>
        </div>
      )}
      {loading && !details && (
        <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
          <Spinner size={12} /> {t('common.loading')}
        </div>
      )}

      {details?.columns && (
        <section className="shrink-0">
          <h3 className="h-7 border-b border-border bg-muted/50 px-3 text-xs font-semibold leading-7 text-muted-foreground">
            {t('table.columns')} ({details.columns.length})
          </h3>
          <div className="overflow-x-auto">
            <Table columns={cols.columns} rows={details.columns} rowKey={(c) => String(c.position)} stickyHeader={false} />
          </div>
        </section>
      )}
      {details?.indexes && (
        <section className="shrink-0">
          <h3 className="h-7 border-b border-border bg-muted/50 px-3 text-xs font-semibold leading-7 text-muted-foreground">
            {t('table.indexes')} ({details.indexes.length})
          </h3>
          <div className="overflow-x-auto">
            <Table columns={cols.indexes} rows={details.indexes} rowKey={(i) => i.name} empty={t('table.noIndexes')} stickyHeader={false} />
          </div>
        </section>
      )}
      {details?.ddl !== undefined && (
        <section className="shrink-0">
          <h3 className="flex h-7 items-center border-b border-border bg-muted/50 px-3 text-xs font-semibold text-muted-foreground">
            {t('table.ddl')}
            <Button size="sm" variant="ghost" className="ml-auto" icon={<Copy size={12} />} onClick={() => void copyDdl()}>
              {copied ? t('common.copied') : t('table.copyDdl')}
            </Button>
          </h3>
          <pre className="overflow-auto p-3 font-mono text-xs leading-5">{details.ddl}</pre>
        </section>
      )}
    </div>
  );
}
