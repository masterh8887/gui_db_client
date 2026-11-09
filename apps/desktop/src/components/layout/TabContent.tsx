'use client';

// Tab content (DESIGN.md §4): every query-target tab of the workspace stays mounted and the
// inactive ones are `hidden` (CodeMirror undo, selection and scroll survive); structure and
// history tabs unmount when inactive.

import { useShallow } from 'zustand/react/shallow';
import { HistoryView } from '@/components/history/HistoryView';
import { QueryTabView } from '@/components/query/QueryTabView';
import { ScriptTabView } from '@/components/script/ScriptTabView';
import { selectTabList, useTabsStore, type WorkspaceTab } from '@/stores/tabs';
import { EmptyWorkspace } from './EmptyWorkspace';
import { TableTabFrame } from './TableTabFrame';

function keepMounted(tab: WorkspaceTab): boolean {
  return tab.kind === 'query' || tab.kind === 'script' || (tab.kind === 'table' && tab.view === 'query');
}

export function TabContent({ connId }: { connId: string }) {
  const list = useTabsStore(useShallow((s) => selectTabList(s, connId)));
  const activeId = useTabsStore((s) => s.sets[connId]?.activeId ?? null);
  if (list.length === 0) return <EmptyWorkspace />;
  return (
    <div className="relative min-h-0 flex-1">
      {list.map((tab) => {
        const active = tab.id === activeId;
        if (!active && !keepMounted(tab)) return null;
        return (
          <div key={tab.id} role="tabpanel" hidden={!active} className="absolute inset-0 flex flex-col bg-window">
            {tab.kind === 'query' ? (
              <QueryTabView tabId={tab.id} />
            ) : tab.kind === 'table' ? (
              <TableTabFrame tab={tab} />
            ) : tab.kind === 'script' ? (
              <ScriptTabView tabId={tab.id} />
            ) : (
              <HistoryView tabId={tab.id} connId={tab.connId} />
            )}
          </div>
        );
      })}
    </div>
  );
}
