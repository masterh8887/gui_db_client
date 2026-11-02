'use client';

// Workspace toolbar (DESIGN.md §5.C "Toolbar"): 40px; left sidebar toggle + connection
// switcher, centre status pill, right JobChip, refresh, palette, console, inspector, mock chip.
// The switcher menu also holds 새 창 and 데이터베이스 복사… (docs/db-copy.md 1.1): the workspace
// entry points on Windows, which has no menu bar.

import { Command, PanelLeft, PanelRight, RotateCw, Server, TriangleAlert } from 'lucide-react';
import { useRef, useState, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { runAction } from '@/lib/actions';
import { newConnection } from '@/lib/connection-actions';
import { connTag } from '@/lib/env';
import { formatClock, num } from '@/lib/format';
import { useInTauri } from '@/lib/ipc';
import { jobProgressPercent } from '@/lib/job-state';
import { useIsMac } from '@/lib/platform';
import { useExecDatabase } from '@/lib/table-target';
import { JobChip } from '@/components/monitor/JobChip';
import { LogStatusButton } from '@/components/log/LogStatusButton';
import { Badge, EnvTag, Spinner, StatusPill, Toolbar, ToolbarButton, Tooltip, type MenuItem } from '@/components/ui';
import { selectDriver, useConnectionsStore } from '@/stores/connections';
import { selectPrimaryActiveJob, useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { connIdsInOtherWindows, useWindowsStore } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { StatusPillPopover } from './StatusPillPopover';
import { ls } from './strings';

/** The pill's text after the connection name, its state and its progress line. */
function usePillModel(connId: string) {
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === connId));
  const driverName = useConnectionsStore((s) => selectDriver(s, conn?.driver)?.name ?? conn?.driver ?? '');
  const version = useConnectionsStore((s) => s.lastProbe[connId]?.server.version);
  const status = useConnectionsStore((s) => s.status[connId] ?? 'unknown');
  const execDb = useExecDatabase(connId);
  const scope = useWorkspaceStore((s) => s.scope[connId]);
  const job = useJobsStore((s) => selectPrimaryActiveJob(s, connId));

  const state = status === 'testing' ? 'connecting' : status === 'error' ? 'error' : status === 'ok' ? 'ok' : 'idle';
  let detail: ReactNode;
  let progress: number | 'indeterminate' | null = null;
  if (job) {
    const p = job.progress;
    if (job.summary.state === 'warned') {
      detail = (
        <span className="flex items-center gap-1 text-warning">
          <TriangleAlert size={12} strokeWidth={2} />
          {ls('warnedShort')}
        </span>
      );
    } else {
      const pct = jobProgressPercent(p);
      const eta = p?.etaSec !== undefined ? ` · ${ls('eta', { eta: formatClock(num(p.etaSec)) })}` : '';
      detail = (
        <span className="flex min-w-0 items-center gap-1.5">
          <Spinner size={11} tone="current" />
          <span className="truncate">{`${job.summary.label}${pct !== undefined ? ` ${Math.round(pct)}%` : ''}${eta}`}</span>
        </span>
      );
      progress = pct ?? 'indeterminate';
    }
  } else {
    const parts = [`${driverName}${version ? ` ${version}` : ''}`];
    if (execDb) parts.push(execDb);
    if (execDb && scope?.database === execDb && scope.schema) parts.push(scope.schema);
    detail = parts.join(' : ');
  }
  return { conn, state: state as 'idle' | 'connecting' | 'ok' | 'error', detail, progress };
}

function ConnectionSwitcher({ connId }: { connId: string }) {
  const connections = useConnectionsStore((s) => s.connections);
  const opened = useWorkspaceStore(useShallow((s) => s.opened));
  const windows = useWindowsStore(useShallow((s) => ({ self: s.self, windows: s.windows })));
  const elsewhere = connIdsInOtherWindows(windows);
  const windowIndex = (connId: string) => windows.windows.find((w) => w.connId === connId && w.label !== windows.self?.label)?.index;
  const items: MenuItem[] = [
    ...connections.map((c) => ({
      id: `conn-${c.id}`,
      icon: <EnvTag tag={connTag(c)} variant="dot" />,
      label: (
        <span className="flex items-center gap-2">
          <span className="truncate">{c.name}</span>
          {opened.includes(c.id) && <span className="text-sm opacity-70">{ls('open')}</span>}
          {elsewhere.includes(c.id) && <span className="text-sm opacity-70">{ls('openInWindow', { index: windowIndex(c.id) ?? '' })}</span>}
        </span>
      ),
      checked: c.id === connId,
      onSelect: () => void useWorkspaceStore.getState().openWorkspace(c.id),
    })),
    {
      id: 'welcome',
      label: ls('allConnections'),
      separatorBefore: true,
      onSelect: () => useWorkspaceStore.getState().showWelcome(),
    },
    { id: 'new', label: ls('newConnection'), shortcut: 'mod+n', onSelect: newConnection },
    { id: 'newWindow', label: ls('newWindow'), shortcut: 'mod+shift+n', separatorBefore: true, onSelect: () => void runAction('window.new') },
    { id: 'copyDatabase', label: ls('copyDatabase'), onSelect: () => void runAction('copy.open') },
  ];
  return <ToolbarButton icon={<Server />} label={ls('switchConnection')} shortcut="mod+shift+o" menu={items} />;
}

export function WorkspaceToolbar({ connId }: { connId: string }) {
  const sidebarOpen = useUiStore((s) => s.sidebarOpen);
  const inspectorOpen = useUiStore((s) => s.inspectorOpen);
  const pill = usePillModel(connId);
  const pillRef = useRef<HTMLButtonElement>(null);
  const [popover, setPopover] = useState(false);
  const mac = useIsMac();
  const tauri = useInTauri();

  return (
    <Toolbar
      dragRegion={mac}
      className={mac ? 'pl-[78px]' : undefined}
      left={
        <>
          <ToolbarButton
            icon={<PanelLeft />}
            label={ls('toggleSidebar')}
            shortcut="mod+0"
            active={sidebarOpen}
            onClick={() => void runAction('view.toggleSidebar')}
          />
          <ConnectionSwitcher connId={connId} />
        </>
      }
      center={
        pill.conn && (
          <>
            <StatusPill
              ref={pillRef}
              tag={connTag(pill.conn)}
              name={pill.conn.name}
              detail={pill.detail}
              state={pill.state}
              progress={pill.progress}
              onClick={() => setPopover((o) => !o)}
            />
            <StatusPillPopover connId={connId} open={popover} onOpenChange={setPopover} anchor={pillRef} />
          </>
        )
      }
      right={
        <>
          <JobChip variant="toolbar" />
          <ToolbarButton icon={<RotateCw />} label={ls('refresh')} shortcut="mod+r" onClick={() => void runAction('catalog.refresh')} />
          <ToolbarButton icon={<Command />} label={ls('palette')} shortcut="mod+p" onClick={() => void runAction('palette.open')} />
          <LogStatusButton />
          <ToolbarButton
            icon={<PanelRight />}
            label={ls('inspector')}
            shortcut="mod+\"
            active={inspectorOpen}
            onClick={() => void runAction('view.toggleInspector')}
          />
          {!tauri && (
            <Tooltip content={ls('mockTip')}>
              <Badge tone="warning" className="ml-1">
                mock
              </Badge>
            </Tooltip>
          )}
        </>
      }
    />
  );
}
