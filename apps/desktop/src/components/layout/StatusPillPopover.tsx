'use client';

// The status pill's popover (DESIGN.md §5.C): connection details, this connection's active jobs,
// active copies into this connection ("이 연결로 복사 중 · {label}"), the other-instances chip
// (docs/db-copy.md 1.2), the write-permission warning, and the connection actions.

import type { RefObject } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { editConnection, testConnection } from '@/lib/connection-actions';
import { connTag } from '@/lib/env';
import { formatBytes } from '@/lib/format';
import { showActivity } from '@/lib/job-actions';
import { useExecDatabase } from '@/lib/table-target';
import { Button, EnvTag, Notice, Popover, StateChip } from '@/components/ui';
import { InstancesChip } from '@/components/welcome/InstancesChip';
import { selectDriver, useConnectionsStore } from '@/stores/connections';
import { isActiveState, selectJobsForConn, useJobsStore, type JobEntry, type JobsState } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { ls } from './strings';

function Row({ label, value }: { label: string; value: string | undefined | null }) {
  if (!value) return null;
  return (
    <>
      <dt className="text-sm text-fg-secondary">{label}</dt>
      <dd className="min-w-0 truncate text-base" data-selectable title={value}>
        {value}
      </dd>
    </>
  );
}

/** Active copy jobs whose target is `connId` (any window of this process started them). */
export function selectCopiesInto(s: JobsState, connId: string): JobEntry[] {
  return Object.values(s.jobs).filter((e) => e.summary.kind === 'copy' && e.summary.copy?.targetConnId === connId && isActiveState(e.summary.state));
}

export function StatusPillPopover({
  connId,
  open,
  onOpenChange,
  anchor,
}: {
  connId: string;
  open: boolean;
  onOpenChange: (o: boolean) => void;
  anchor: RefObject<HTMLElement | null>;
}) {
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === connId));
  const driver = useConnectionsStore((s) => selectDriver(s, conn?.driver));
  const probe = useConnectionsStore((s) => s.lastProbe[connId]);
  const error = useConnectionsStore((s) => (s.status[connId] === 'error' ? s.errors[connId] : undefined));
  const jobs = useJobsStore(useShallow((s) => selectJobsForConn(s, connId).filter((e) => isActiveState(e.summary.state))));
  const copiesInto = useJobsStore(useShallow((s) => selectCopiesInto(s, connId)));
  const execDb = useExecDatabase(connId);
  const browsing = useWorkspaceStore((s) => s.scope[connId]?.database ?? null);
  if (!conn) return null;

  const close = () => onOpenChange(false);
  const host = conn.fields.path ?? (conn.host ? `${conn.host}${conn.port ? `:${conn.port}` : ''}` : undefined);
  const ssh = conn.ssh?.enabled ? `${conn.ssh.user}@${conn.ssh.host}:${conn.ssh.port}` : ls('sshOff');
  const ram = probe?.server.ramBytes !== undefined ? formatBytes(probe.server.ramBytes) : undefined;

  return (
    <Popover open={open} onOpenChange={onOpenChange} anchor={anchor} align="center" width={360} aria-label={ls('pillLabel')}>
      <div className="flex flex-col gap-3 p-4">
        <div className="flex items-center gap-2">
          <span className="min-w-0 truncate text-lg font-semibold">{conn.name}</span>
          <EnvTag tag={connTag(conn)} variant="badge" />
          <span className="ml-auto">
            <InstancesChip />
          </span>
        </div>

        {jobs.length > 0 && (
          <div className="flex flex-col gap-1.5">
            <div className="text-sm font-semibold text-fg-secondary">{ls('activeJobs')}</div>
            {jobs.map((j) => (
              <div key={j.summary.id} className="flex items-center gap-2">
                <StateChip state={j.summary.state} />
                <span className="min-w-0 truncate">{j.summary.label}</span>
              </div>
            ))}
            <Button
              size="sm"
              variant="secondary"
              className="self-start"
              onClick={() => {
                showActivity();
                close();
              }}
            >
              {ls('showActivity')}
            </Button>
          </div>
        )}

        {copiesInto.length > 0 && (
          <div className="flex flex-col gap-1.5" data-copies-into="">
            <div className="text-sm font-semibold text-fg-secondary">{ls('copyingIntoTitle')}</div>
            {copiesInto.map((j) => (
              <div key={j.summary.id} className="flex items-center gap-2">
                <StateChip state={j.summary.state} />
                <span className="min-w-0 truncate">{ls('copyingInto', { label: j.summary.label })}</span>
              </div>
            ))}
          </div>
        )}

        <dl className="grid grid-cols-[112px_1fr] items-baseline gap-x-3 gap-y-1">
          <Row label={ls('driver')} value={driver?.name ?? conn.driver} />
          <Row label={ls('serverVersion')} value={probe?.server.version ?? ls('noVersion')} />
          <Row label={ls('host')} value={host} />
          <Row label={ls('execDatabase')} value={execDb} />
          {browsing && browsing !== execDb && <Row label={ls('browsing')} value={browsing} />}
          <Row label={ls('user')} value={probe?.server.user ?? conn.user} />
          <Row label={ls('ssh')} value={ssh} />
          <Row label={ls('serverRam')} value={ram} />
        </dl>

        {probe?.server.canWrite && <Notice tone="warning">{ls('canWrite')}</Notice>}
        {error && <Notice tone="danger">{error}</Notice>}

        <div className="flex flex-wrap gap-1.5">
          <Button size="sm" onClick={() => void testConnection(connId)}>
            {ls('testConnection')}
          </Button>
          <Button
            size="sm"
            onClick={() => {
              close();
              editConnection(connId);
            }}
          >
            {ls('editConnection')}
          </Button>
          <Button
            size="sm"
            onClick={() => {
              close();
              useUiStore.getState().openPalette('connections');
            }}
          >
            {ls('otherConnection')}
          </Button>
        </div>
      </div>
      {/* Footer: app settings on the left, the destructive close on the right (not mixed into
          the row of connection actions). */}
      <div className="flex items-center justify-between border-t border-separator py-1.5 pr-2 pl-4">
        <Button
          variant="link"
          size="sm"
          onClick={() => {
            close();
            useUiStore.getState().setSettingsOpen(true);
          }}
        >
          {ls('appSettings')}
        </Button>
        <Button
          size="sm"
          variant="danger"
          onClick={() => {
            close();
            void useWorkspaceStore.getState().closeWorkspace(connId);
          }}
        >
          {ls('closeWorkspace')}
        </Button>
      </div>
    </Popover>
  );
}
