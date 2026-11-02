'use client';

// 세부 정보 in the inspector (DESIGN.md §5.C, §7.6): one table's estimate (추정), exact count
// with its date, sizes, comment, and the columns and indexes the catalog store already has.
// `source: 'selection'` (the sidebar selection) makes no IPC call at all: arrowing through the
// sidebar never loads anything. `source: 'tab'` (the active table tab) loads the details and the
// stored last exact count, like the structure view.

import type { TableRef } from 'ipc-types';
import { Calculator, FileCode2, KeyRound, PanelTop } from 'lucide-react';
import { useEffect } from 'react';
import { Badge, Button, EmptyState, RowEstimate, Spinner, Tooltip } from '@/components/ui';
import { formatBytes } from '@/lib/format';
import { errorText } from '@/lib/ipc-error';
import { canTargetTable, countExactly, openExtractionQuery, useExecDatabase } from '@/lib/table-actions';
import { tableKey, tableLabel } from '@/lib/utils';
import { useCatalogStore } from '@/stores/catalog';
import { logLocal } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { Prop, PropList, PropSection } from './OverviewPanel';
import { ts } from './strings';
import { ExactCountValue, KindIcon, kindLabel, useDetailsMissing } from './table-ui';

export function TableInspector({ connId, table, source }: { connId: string; table: TableRef | null; source: 'tab' | 'selection' }) {
  if (!table) return <EmptyState title={ts('insp.none')} />;
  return <InspectorBody connId={connId} table={table} source={source} />;
}

const dash = <span className="text-fg-tertiary">—</span>;

function InspectorBody({ connId, table, source }: { connId: string; table: TableRef; source: 'tab' | 'selection' }) {
  const key = tableKey(connId, table);
  const info = useCatalogStore((s) => s.findTable(connId, table));
  const details = useCatalogStore((s) => s.details[key]);
  const loading = useCatalogStore((s) => !!s.loading[key]);
  const exact = useCatalogStore((s) => s.exactCounts[key]);
  const counting = useCatalogStore((s) => !!s.counting[key]);
  const detailsMissing = useDetailsMissing(connId, key);
  useExecDatabase(connId);
  const target = canTargetTable(connId, table);

  useEffect(() => {
    if (source !== 'tab') return;
    const catalog = useCatalogStore.getState();
    void catalog.loadTableDetails(connId, table).catch(() => undefined);
    void catalog.loadLastExactCount(connId, table).catch((e: unknown) => logLocal('warn', `catalog_last_exact_count: ${errorText(e)}`));
  }, [source, connId, table]);

  // A catalog refresh drops the cached details: the active tab's inspector loads them again
  // (the selection never loads anything).
  useEffect(() => {
    if (source !== 'tab' || !detailsMissing) return;
    void useCatalogStore
      .getState()
      .loadTableDetails(connId, table)
      .catch(() => undefined);
  }, [source, detailsMissing, connId, table]);

  const openStructure = () => useTabsStore.getState().openTable(connId, table, { preview: false, view: 'structure' });
  const columns = details?.columns;
  const indexes = details?.indexes;

  return (
    <div role="group" aria-label={ts('insp.label', { table: tableLabel(table) })} data-source={source} className="flex flex-col gap-4 px-3 py-3">
      <div className="flex min-w-0 flex-col gap-1">
        <div className="flex min-w-0 items-center gap-2">
          <KindIcon kind={info?.kind} />
          <span className="min-w-0 truncate text-base font-semibold">{table.name}</span>
          {info && (
            <Badge tone="neutral" hangul>
              {kindLabel(info.kind)}
            </Badge>
          )}
        </div>
        {(table.schema || table.database) && (
          <div className="truncate text-sm text-fg-secondary">
            {table.schema}
            {table.database && ` @ ${table.database}`}
          </div>
        )}
      </div>

      <div className="flex flex-wrap gap-1.5">
        {source === 'selection' ? (
          <Button size="sm" variant="secondary" icon={<PanelTop size={14} strokeWidth={1.75} />} onClick={openStructure}>
            {ts('insp.open')}
          </Button>
        ) : null}
        <Tooltip content={target.ok ? ts('makeQueryTip') : target.reason}>
          <span className="inline-flex">
            <Button
              size="sm"
              variant="secondary"
              icon={<FileCode2 size={14} strokeWidth={1.75} />}
              disabled={!target.ok}
              onClick={() => void openExtractionQuery(connId, table)}
            >
              {ts('makeQuery')}
            </Button>
          </span>
        </Tooltip>
        <Button
          size="sm"
          variant="secondary"
          icon={<Calculator size={14} strokeWidth={1.75} />}
          loading={counting}
          onClick={() => void countExactly(connId, table)}
        >
          {counting ? ts('counting') : ts('countExactly')}
        </Button>
      </div>

      <PropSection title={ts('ov.rows')}>
        <PropList>
          <Prop label={ts('ov.estimated')}>
            <RowEstimate rows={info?.estimatedRows} variant="full" />
          </Prop>
          <Prop label={ts('ov.exact')}>{exact ? <ExactCountValue exact={exact} /> : <span aria-label={ts('ov.notCounted')}>{dash}</span>}</Prop>
        </PropList>
        {!info && <p className="text-sm text-fg-secondary">{ts('insp.notInCatalog')}</p>}
      </PropSection>

      <PropSection title={ts('ov.size')}>
        <PropList>
          <Prop label={ts('ov.totalSize')}>{info?.totalBytes !== undefined ? <span className="tabular-nums">{formatBytes(info.totalBytes)}</span> : dash}</Prop>
          <Prop label={ts('ov.tableSize')}>{info?.tableBytes !== undefined ? <span className="tabular-nums">{formatBytes(info.tableBytes)}</span> : dash}</Prop>
          <Prop label={ts('ov.indexSize')}>{info?.indexBytes !== undefined ? <span className="tabular-nums">{formatBytes(info.indexBytes)}</span> : dash}</Prop>
        </PropList>
      </PropSection>

      {info?.comment && (
        <PropSection title={ts('ov.comment')}>
          <p className="whitespace-pre-wrap text-base" data-selectable>
            {info.comment}
          </p>
        </PropSection>
      )}

      <PropSection title={columns ? `${ts('insp.columns')} ${columns.length}` : ts('insp.columns')}>
        {columns ? (
          <ul aria-label={ts('insp.columns')} className="flex flex-col">
            {columns.map((c) => (
              <li key={`${c.position}:${c.name}`} className="flex h-(--h-row-sm) min-w-0 items-center gap-1.5">
                <span className="inline-flex w-3 shrink-0 justify-center">
                  {c.primaryKey && <KeyRound size={12} strokeWidth={2} role="img" aria-label={ts('primaryKey')} className="text-fg-secondary" />}
                </span>
                <span className="min-w-0 truncate">{c.name}</span>
                <span className="ml-auto shrink-0 truncate pl-2 font-mono text-sm text-fg-secondary">{c.dataType}</span>
              </li>
            ))}
          </ul>
        ) : source === 'tab' && loading ? (
          <div role="status" className="flex items-center gap-2 text-sm text-fg-secondary">
            <Spinner size={12} /> {ts('loading')}
          </div>
        ) : (
          <p className="text-sm text-fg-secondary">{ts('insp.notCached')}</p>
        )}
      </PropSection>

      {indexes && (
        <PropSection title={`${ts('insp.indexes')} ${indexes.length}`}>
          {indexes.length === 0 ? (
            <p className="text-sm text-fg-secondary">{ts('noIndexes')}</p>
          ) : (
            <ul aria-label={ts('insp.indexes')} className="flex flex-col">
              {indexes.map((i) => (
                <li key={i.name} className="flex h-(--h-row-sm) min-w-0 items-center gap-1.5">
                  <span className="min-w-0 truncate">{i.name}</span>
                  {i.primary && <Badge tone="accent">PK</Badge>}
                  {i.unique && !i.primary && <Badge tone="neutral">UNIQUE</Badge>}
                  <span className="ml-auto shrink-0 truncate pl-2 text-sm text-fg-secondary">{i.columns.join(', ')}</span>
                </li>
              ))}
            </ul>
          )}
        </PropSection>
      )}
    </div>
  );
}
