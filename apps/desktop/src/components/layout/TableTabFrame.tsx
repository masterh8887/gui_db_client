'use client';

// Table tab frame (DESIGN.md §5.E): the content (`StructureTabView` or `QueryTabView`) over a
// 28px bottom bar with the 쿼리 | 구조 toggle, the row estimate and the total size.

import { QueryTabView } from '@/components/query/QueryTabView';
import { StructureTabView } from '@/components/table/StructureTabView';
import { RowEstimate, SegmentedControl, StatusBar } from '@/components/ui';
import { formatBytes } from '@/lib/format';
import { canTargetTable, useExecDatabase } from '@/lib/table-target';
import { tableKey } from '@/lib/utils';
import { useCatalogStore } from '@/stores/catalog';
import { useTabsStore, type TableTab, type TableView } from '@/stores/tabs';
import { ls } from './strings';

export function TableTabFrame({ tab }: { tab: TableTab }) {
  const info = useCatalogStore((s) => s.findTable(tab.connId, tab.table));
  const exact = useCatalogStore((s) => s.exactCounts[tableKey(tab.connId, tab.table)]);
  // Re-evaluated when the execution database becomes known.
  useExecDatabase(tab.connId);
  const target = canTargetTable(tab.connId, tab.table);
  const setView = (v: TableView) => void useTabsStore.getState().setTableView(tab.id, v);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="min-h-0 flex-1">
        {tab.view === 'query' ? (
          <QueryTabView tabId={tab.id} />
        ) : (
          <StructureTabView
            tabId={tab.id}
            connId={tab.connId}
            table={tab.table}
            segment={tab.segment}
            onSegmentChange={(s) => useTabsStore.getState().setSegment(tab.id, s)}
          />
        )}
      </div>
      <StatusBar surface="panel">
        <SegmentedControl<TableView>
          aria-label={ls('tableView')}
          value={tab.view}
          onChange={setView}
          options={[
            { value: 'query', label: ls('viewQuery'), disabled: !target.ok, title: target.ok ? undefined : target.reason },
            { value: 'structure', label: ls('viewStructure') },
          ]}
        />
        <span className="ml-auto flex items-center gap-1.5 text-fg-secondary">
          <RowEstimate rows={info?.estimatedRows} exact={exact} variant="full" />
          {info?.totalBytes !== undefined && (
            <>
              <span aria-hidden>·</span>
              <span className="tabular-nums">{formatBytes(info.totalBytes)}</span>
            </>
          )}
        </span>
      </StatusBar>
    </div>
  );
}
