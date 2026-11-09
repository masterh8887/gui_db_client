'use client';

// The workspace tab strip (DESIGN.md §5.C "Tab bar") over `TabBar` / `TabItem`.

import { FileCode2, History, Table2, Terminal } from 'lucide-react';
import { useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { runAction } from '@/lib/actions';
import { tableLabel } from '@/lib/utils';
import { TabBar, TabItem, type MenuItem } from '@/components/ui';
import { isDocDirty, useExtractStore } from '@/stores/extract';
import { isActiveState, useJobsStore } from '@/stores/jobs';
import { isQueryTarget, selectTabList, tabTitle, useTabsStore, type WorkspaceTab } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { ls } from './strings';

function WorkspaceTabItem({
  tab,
  active,
  renaming,
  setRenaming,
}: {
  tab: WorkspaceTab;
  active: boolean;
  renaming: boolean;
  setRenaming: (id: string | null) => void;
}) {
  const doc = useExtractStore(
    useShallow((s) => {
      const d = s.docs[tab.id];
      return d ? { name: d.name, dirty: isDocDirty(d), busy: d.busy !== null, lastJobId: d.lastJobId } : null;
    }),
  );
  const jobActive = useJobsStore((s) => {
    const id = doc?.lastJobId;
    const st = id ? s.jobs[id]?.summary.state : undefined;
    return !!st && isActiveState(st);
  });
  const tabs = useTabsStore.getState;
  const title = tabTitle(tab, doc ?? undefined);
  const preview = tab.kind === 'table' && tab.preview;
  const icon = tab.kind === 'query' ? <FileCode2 strokeWidth={1.75} /> : tab.kind === 'table' ? <Table2 strokeWidth={1.75} /> : tab.kind === 'script' ? <Terminal strokeWidth={1.75} /> : <History strokeWidth={1.75} />;

  const items: MenuItem[] = [
    { id: 'close', label: ls('close'), shortcut: 'mod+w', onSelect: () => tabs().close(tab.id) },
    { id: 'closeOthers', label: ls('closeOthers'), onSelect: () => tabs().closeOthers(tab.id) },
    { id: 'closeRight', label: ls('closeRight'), onSelect: () => tabs().closeToRight(tab.id) },
    { id: 'closeAll', label: ls('closeAll'), onSelect: () => tabs().closeAll(tab.connId) },
    { id: 'rename', label: ls('rename'), separatorBefore: true, onSelect: () => setRenaming(tab.id) },
    ...(preview ? [{ id: 'pin', label: ls('pin'), onSelect: () => tabs().pin(tab.id) }] : []),
    ...(isQueryTarget(tab)
      ? [
          {
            id: 'openIn',
            label: ls('openInConnection'),
            separatorBefore: true,
            onSelect: () => useUiStore.getState().openPalette('connections', '', { kind: 'openInConnection', tabId: tab.id }),
          },
        ]
      : []),
  ];

  return (
    <TabItem
      id={tab.id}
      title={title}
      icon={icon}
      active={active}
      dirty={!!doc?.dirty}
      busy={!!doc?.busy || jobActive}
      preview={preview}
      tooltip={tab.kind === 'table' ? `${tableLabel(tab.table)}${tab.table.database ? ` @ ${tab.table.database}` : ''}` : undefined}
      onActivate={() => tabs().activate(tab.id)}
      onClose={() => tabs().close(tab.id)}
      onDoubleClick={preview ? () => tabs().pin(tab.id) : undefined}
      contextItems={items}
      renaming={renaming}
      onRename={(t) => {
        tabs().rename(tab.id, t);
        setRenaming(null);
      }}
      onRenameCancel={() => setRenaming(null)}
    />
  );
}

export function WorkspaceTabBar({ connId }: { connId: string }) {
  const list = useTabsStore(useShallow((s) => selectTabList(s, connId)));
  const activeId = useTabsStore((s) => s.sets[connId]?.activeId ?? null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const openQuery = () => useTabsStore.getState().openQuery(connId);
  const openScript = () => useTabsStore.getState().openScript(connId);

  return (
    <TabBar
      aria-label={ls('tabs')}
      onNewTab={openQuery}
      onEmptyDoubleClick={openQuery}
      onReorder={(from, to) => useTabsStore.getState().reorder(connId, from, to)}
      newTabContextItems={[
        { id: 'new', label: ls('newQuery'), shortcut: 'mod+t', onSelect: openQuery },
        { id: 'file', label: ls('openSqlFile'), shortcut: 'mod+o', onSelect: () => void runAction('query.openFile') },
        { id: 'script', label: '새 스크립트', onSelect: openScript },
        { id: 'history', label: ls('fullHistory'), onSelect: () => useTabsStore.getState().openHistory(connId) },
      ]}
    >
      {list.map((tab) => (
        <WorkspaceTabItem key={tab.id} tab={tab} active={tab.id === activeId} renaming={renaming === tab.id} setRenaming={setRenaming} />
      ))}
    </TabBar>
  );
}
