'use client';

// Structure view of a table tab (DESIGN.md §5.E): a 36px header (name, kind chip, `@ database`;
// segments 열 | 인덱스 | DDL | 개요; 추출 쿼리, 정확히 세기, refresh) over the segment. Metadata
// only: nothing here ever reads table data. Row counts are estimates (추정) unless they come
// from an exact count, which shows its date: the stored last count is read on mount, and a
// newer `job:done` wins in the catalog store.

import type { TableRef } from 'ipc-types';
import { Calculator, FileCode2, RotateCw } from 'lucide-react';
import { useEffect, type ReactNode } from 'react';
import { Badge, Button, CodeBlock, CopyButton, EmptyState, IconButton, Notice, SegmentedControl, Spinner, Tooltip } from '@/components/ui';
import { errorText } from '@/lib/ipc-error';
import { canTargetTable, countExactly, openExtractionQuery, useExecDatabase } from '@/lib/table-actions';
import { tableKey, tableLabel } from '@/lib/utils';
import { schemaKey, useCatalogStore } from '@/stores/catalog';
import { logLocal } from '@/stores/logs';
import type { StructureSegment } from '@/stores/tabs';
import { OverviewPanel } from './OverviewPanel';
import { ColumnsGrid, IndexesGrid } from './StructureGrids';
import { ts } from './strings';
import { KindIcon, kindLabel, openSelectAllInNewTab, useDetailsMissing } from './table-ui';

const SEGMENTS: StructureSegment[] = ['columns', 'indexes', 'ddl', 'overview'];
const SEGMENT_LABEL: Record<StructureSegment, () => string> = {
  columns: () => ts('segColumns'),
  indexes: () => ts('segIndexes'),
  ddl: () => ts('segDdl'),
  overview: () => ts('segOverview'),
};

/** A header button whose tooltip also explains why it is disabled. */
function HeaderAction({ tip, children }: { tip: string; children: ReactNode }) {
  return (
    <Tooltip content={tip}>
      <span className="inline-flex">{children}</span>
    </Tooltip>
  );
}

export function StructureTabView({
  tabId,
  connId,
  table,
  segment,
  onSegmentChange,
}: {
  tabId: string;
  connId: string;
  table: TableRef;
  segment: StructureSegment;
  onSegmentChange: (s: StructureSegment) => void;
}) {
  const key = tableKey(connId, table);
  const details = useCatalogStore((s) => s.details[key]);
  const loading = useCatalogStore((s) => !!s.loading[key]);
  const error = useCatalogStore((s) => s.errors[key]);
  const info = useCatalogStore((s) => s.findTable(connId, table));
  const exact = useCatalogStore((s) => s.exactCounts[key]);
  const counting = useCatalogStore((s) => !!s.counting[key]);
  // The schema's table list (estimate, sizes, kind) is known unless the tab came from elsewhere.
  const listKey = table.database && table.schema ? schemaKey(connId, table.database, table.schema) : null;
  const listMissing = useCatalogStore((s) => !!listKey && !s.tables[listKey] && !s.loading[listKey] && !s.errors[listKey]);
  const detailsMissing = useDetailsMissing(connId, key);
  // Re-evaluates `canTargetTable` once the execution database is known.
  useExecDatabase(connId);
  const target = canTargetTable(connId, table);

  // On mount and for another table; also retries a load that failed before the tab was reopened.
  useEffect(() => {
    void useCatalogStore
      .getState()
      .loadTableDetails(connId, table)
      .catch(() => undefined);
  }, [connId, table]);

  // A catalog refresh (toolbar, Mod+R / F5, the sidebar, a saved connection edit) drops the
  // cached details of every table of the connection: load them again instead of going blank.
  useEffect(() => {
    if (!detailsMissing) return;
    void useCatalogStore
      .getState()
      .loadTableDetails(connId, table)
      .catch(() => undefined);
  }, [detailsMissing, connId, table]);

  // The last count the user ran (possibly in an earlier session) is stored by the backend.
  useEffect(() => {
    void useCatalogStore
      .getState()
      .loadLastExactCount(connId, table)
      .catch((e: unknown) => logLocal('warn', `catalog_last_exact_count: ${errorText(e)}`));
  }, [connId, table]);

  useEffect(() => {
    if (!listMissing || !table.database || !table.schema) return;
    void useCatalogStore
      .getState()
      .loadTables(connId, table.database, table.schema)
      .catch(() => undefined);
  }, [listMissing, connId, table.database, table.schema]);

  const refresh = () => {
    void useCatalogStore
      .getState()
      .loadTableDetails(connId, table, true)
      .catch(() => undefined);
  };
  const count = () => void countExactly(connId, table);
  const makeQuery = () => void openExtractionQuery(connId, table);

  const segmentAction =
    segment === 'columns' && details?.columns?.length ? (
      <CopyButton label={ts('copyColumns')} getText={() => (details.columns ?? []).map((c) => c.name).join(', ')} />
    ) : segment === 'ddl' && details?.ddl ? (
      <CopyButton label={ts('copyDdl')} getText={() => details.ddl ?? ''} />
    ) : null;

  let body: ReactNode;
  const pending = !details && loading;
  if (segment === 'overview') {
    body = (
      <OverviewPanel
        table={table}
        info={info}
        details={details}
        exact={exact}
        counting={counting}
        canQuery={target}
        onCount={count}
        onOpenQueryTab={() => void openSelectAllInNewTab(connId, table)}
      />
    );
  } else if (pending) {
    body = (
      <div role="status" className="flex h-(--h-row) items-center gap-2 px-3 text-sm text-fg-secondary">
        <Spinner size={12} /> {ts('loading')}
      </div>
    );
  } else if (segment === 'columns') {
    body = details?.columns ? <ColumnsGrid connId={connId} table={table} columns={details.columns} /> : null;
  } else if (segment === 'indexes') {
    body = details?.indexes ? <IndexesGrid table={table} indexes={details.indexes} /> : null;
  } else {
    body =
      details?.ddl !== undefined ? (
        details.ddl ? (
          <CodeBlock flush code={details.ddl} aria-label={ts('ddlLabel', { table: tableLabel(table) })} className="h-full" />
        ) : (
          <EmptyState title={ts('noDdl')} />
        )
      ) : null;
  }

  return (
    <div className="@container flex h-full min-h-0 flex-col bg-window" data-tab={tabId}>
      <header className="flex h-9 shrink-0 items-stretch gap-4 border-b border-separator pr-2 pl-3">
        <div className="flex min-w-0 shrink items-center gap-2">
          <KindIcon kind={info?.kind} />
          <h2 className="flex min-w-0 items-baseline truncate text-base font-semibold">
            {table.schema && <span className="hidden font-normal text-fg-secondary @2xl:inline">{table.schema}.</span>}
            <span className="truncate">{table.name}</span>
          </h2>
          {info && (
            <Badge tone="neutral" hangul>
              {kindLabel(info.kind)}
            </Badge>
          )}
          {table.database && <span className="hidden shrink-0 truncate text-sm text-fg-secondary @xl:inline">@ {table.database}</span>}
        </div>
        <SegmentedControl<StructureSegment>
          variant="tabs"
          aria-label={ts('sections')}
          value={segment}
          onChange={onSegmentChange}
          options={SEGMENTS.map((s) => ({ value: s, label: SEGMENT_LABEL[s]() }))}
        />
        <div className="ml-auto flex shrink-0 items-center gap-0.5">
          {segmentAction}
          {segmentAction && <span aria-hidden className="mx-1 h-3.5 w-px bg-separator-strong" />}
          <HeaderAction tip={target.ok ? ts('makeQueryTip') : target.reason}>
            <Button size="sm" variant="ghost" icon={<FileCode2 size={14} strokeWidth={1.75} />} disabled={!target.ok} onClick={makeQuery}>
              <span className="@max-[700px]:sr-only">{ts('makeQuery')}</span>
            </Button>
          </HeaderAction>
          <HeaderAction tip={ts('countTip')}>
            <Button
              size="sm"
              variant="ghost"
              icon={<Calculator size={14} strokeWidth={1.75} />}
              loading={counting}
              onClick={count}
            >
              <span className="@max-[700px]:sr-only">{counting ? ts('counting') : ts('countExactly')}</span>
            </Button>
          </HeaderAction>
          <IconButton size="sm" label={ts('refresh')} onClick={refresh} disabled={loading}>
            {loading && details ? <Spinner size={12} tone="current" /> : <RotateCw size={14} strokeWidth={1.75} />}
          </IconButton>
        </div>
      </header>
      {error && (
        <div className="shrink-0 px-3 pt-2">
          <Notice tone="danger">
            <span className="flex items-center gap-2">
              <span className="min-w-0 flex-1">{error}</span>
              <Button size="sm" variant="secondary" onClick={refresh}>
                {ts('retry')}
              </Button>
            </span>
          </Notice>
        </div>
      )}
      <div role="tabpanel" aria-label={SEGMENT_LABEL[segment]()} className="min-h-0 flex-1">
        {body}
      </div>
    </div>
  );
}
