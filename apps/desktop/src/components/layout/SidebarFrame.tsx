'use client';

// Left sidebar frame (DESIGN.md §5.C "Sidebar"): `bg-sidebar`, right hairline, resizable; a
// 28px top row with the 항목 | 기록 segmented control, then the `Sidebar` or `HistoryList` slot.

import { HistoryList } from '@/components/history/HistoryList';
import { Sidebar } from '@/components/sidebar/Sidebar';
import { SegmentedControl } from '@/components/ui';
import { useUiStore, type SidebarSegment } from '@/stores/ui';
import { ResizeHandle } from './ResizeHandle';
import { ls } from './strings';

export function SidebarFrame({ connId }: { connId: string }) {
  const width = useUiStore((s) => s.leftWidth);
  const setWidth = useUiStore((s) => s.setLeftWidth);
  const segment = useUiStore((s) => s.sidebarSegment);
  const setSegment = useUiStore((s) => s.setSidebarSegment);
  return (
    <aside
      data-surface="sidebar"
      aria-label={ls('toggleSidebar')}
      className="relative flex h-full shrink-0 flex-col border-r border-separator bg-sidebar"
      style={{ width }}
    >
      <div className="flex h-(--h-bar) shrink-0 items-center px-2 pt-1.5 pb-0.5">
        <SegmentedControl<SidebarSegment>
          aria-label={ls('sidebarView')}
          value={segment}
          onChange={setSegment}
          fill
          options={[
            { value: 'items', label: ls('sidebarItems') },
            { value: 'history', label: ls('sidebarHistory') },
          ]}
        />
      </div>
      <div className="min-h-0 flex-1">{segment === 'items' ? <Sidebar connId={connId} /> : <HistoryList connId={connId} />}</div>
      <ResizeHandle edge="right" size={width} onResize={setWidth} label={ls('resizeSidebar')} />
    </aside>
  );
}
