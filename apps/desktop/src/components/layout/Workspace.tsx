'use client';

// One connection's workspace (DESIGN.md §4, §5.C): toolbar, then sidebar │ tabs + content │
// inspector (closed by default).

import { useUiStore } from '@/stores/ui';
import { InspectorFrame } from './InspectorFrame';
import { SidebarFrame } from './SidebarFrame';
import { TabContent } from './TabContent';
import { WorkspaceTabBar } from './WorkspaceTabBar';
import { WorkspaceToolbar } from './WorkspaceToolbar';

export function Workspace({ connId }: { connId: string }) {
  const sidebarOpen = useUiStore((s) => s.sidebarOpen);
  const inspectorOpen = useUiStore((s) => s.inspectorOpen);
  return (
    <div className="flex h-full min-h-0 flex-col" data-workspace={connId}>
      <WorkspaceToolbar connId={connId} />
      <div className="flex min-h-0 flex-1">
        {sidebarOpen && <SidebarFrame connId={connId} />}
        <main className="flex min-w-0 flex-1 flex-col bg-window">
          <WorkspaceTabBar connId={connId} />
          <TabContent connId={connId} />
        </main>
        {inspectorOpen && <InspectorFrame connId={connId} />}
      </div>
    </div>
  );
}
