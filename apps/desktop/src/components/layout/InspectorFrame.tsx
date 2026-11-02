'use client';

// Right inspector frame (DESIGN.md §5.C "Inspector"): closed by default, resizable; 28px tabs
// header 활동 (badge = active job count) | 세부 정보 → ActivityPanel / TableInspector.

import type { TableRef } from 'ipc-types';
import { useShallow } from 'zustand/react/shallow';
import { ActivityPanel } from '@/components/monitor/ActivityPanel';
import { TableInspector } from '@/components/table/TableInspector';
import { EmptyState, InspectorPanel } from '@/components/ui';
import { selectActiveJobs, useJobsStore } from '@/stores/jobs';
import { selectActiveTab, useTabsStore } from '@/stores/tabs';
import { useUiStore, type InspectorSegment } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { ResizeHandle } from './ResizeHandle';
import { ls } from './strings';

/**
 * TableInspector target: the active table tab's table (`tab`) unless that tab shows Structure
 * 개요 (then the sidebar selection, else a hint); with no table tab, the sidebar selection.
 */
export function useInspectorTarget(connId: string): { table: TableRef | null; source: 'tab' | 'selection'; inOverview: boolean } {
  const tab = useTabsStore((s) => selectActiveTab(s, connId));
  const selection = useWorkspaceStore((s) => s.selection[connId] ?? null);
  if (tab?.kind === 'table') {
    const overview = tab.view === 'structure' && tab.segment === 'overview';
    if (!overview) return { table: tab.table, source: 'tab', inOverview: false };
    return { table: selection, source: 'selection', inOverview: !selection };
  }
  return { table: selection, source: 'selection', inOverview: false };
}

export function InspectorFrame({ connId }: { connId: string }) {
  const width = useUiStore((s) => s.rightWidth);
  const setWidth = useUiStore((s) => s.setRightWidth);
  const segment = useUiStore((s) => s.inspectorSegment);
  const setSegment = useUiStore((s) => s.setInspectorSegment);
  const activeCount = useJobsStore(useShallow(selectActiveJobs)).length;
  const target = useInspectorTarget(connId);

  return (
    <aside aria-label={ls('inspector')} className="relative h-full shrink-0 border-l border-separator" style={{ width }}>
      <ResizeHandle edge="left" size={width} onResize={setWidth} label={ls('resizeInspector')} />
      <InspectorPanel
        aria-label={ls('inspector')}
        segment={segment}
        onSegmentChange={(s) => setSegment(s as InspectorSegment)}
        segments={[
          {
            value: 'activity',
            label: ls('activity'),
            badge:
              activeCount > 0 ? (
                <span className="rounded-full bg-accent-soft px-1.5 text-xs font-medium tabular-nums text-accent">{activeCount}</span>
              ) : undefined,
          },
          { value: 'details', label: ls('details') },
        ]}
      >
        {segment === 'activity' ? (
          <ActivityPanel variant="inspector" />
        ) : target.inOverview ? (
          <EmptyState title={ls('detailsInOverview')} />
        ) : target.table ? (
          <TableInspector connId={connId} table={target.table} source={target.source} />
        ) : (
          <EmptyState title={ls('noTableSelected')} />
        )}
      </InspectorPanel>
    </aside>
  );
}
