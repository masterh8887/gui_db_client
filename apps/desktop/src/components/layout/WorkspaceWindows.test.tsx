// The workspace side of several windows / instances and DB copy (docs/db-copy.md 1.1, 1.2): the
// connection switcher's 새 창 / 데이터베이스 복사… entries and the status pill popover's
// "이 연결로 복사 중" list and other-instances chip.

import type { JobSummary } from 'ipc-types';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useRef } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { registerBuiltinActions } from '@/lib/actions';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { TooltipProvider } from '@/components/ui';
import { ws } from '@/components/welcome/strings';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { useWindowsStore } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { selectCopiesInto, StatusPillPopover } from './StatusPillPopover';
import { ls } from './strings';
import { WorkspaceToolbar } from './WorkspaceToolbar';

function copyJob(id: string, target: string, state: JobSummary['state'] = 'running'): JobSummary {
  return {
    id,
    kind: 'copy',
    state,
    connId: 'c-prod',
    connName: 'prod',
    label: `copy ${id}`,
    startedAt: '2026-10-27T10:00:00Z',
    copy: {
      runId: id,
      targetConnId: target,
      targetConnName: target,
      sourceDatabase: 'orders',
      targetDatabase: 'orders',
      phase: 'data',
      tablesTotal: 2,
      tablesDone: 0,
      tablesFailed: 0,
      dryRun: false,
    },
  };
}

function seed(...jobs: JobSummary[]) {
  useJobsStore.setState({ jobs: Object.fromEntries(jobs.map((j) => [j.id, { summary: j, logs: [], updatedAt: 0 }])), order: jobs.map((j) => j.id) });
}

function Pill({ connId }: { connId: string }) {
  const anchor = useRef<HTMLDivElement>(null);
  return (
    <TooltipProvider>
      <div ref={anchor} />
      <StatusPillPopover connId={connId} open onOpenChange={() => undefined} anchor={anchor} />
    </TooltipProvider>
  );
}

describe('workspace entry points for windows and copy', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });

  it('selectCopiesInto keeps active copy jobs whose target is the connection', () => {
    seed(copyJob('a', 'c-staging'), copyJob('b', 'c-staging', 'done'), copyJob('c', 'c-local'), { ...copyJob('d', 'c-staging'), kind: 'export' });
    expect(selectCopiesInto(useJobsStore.getState(), 'c-staging').map((e) => e.summary.id)).toEqual(['a']);
    expect(selectCopiesInto(useJobsStore.getState(), 'c-prod')).toEqual([]);
  });

  it('the pill popover lists copies into this connection and other instances', () => {
    seed(copyJob('a', 'c-staging'));
    useWindowsStore.setState({
      instances: { selfPid: 1, others: [{ pid: 7, instanceId: 'i', kind: 'cli', startedAt: '2026-10-27T10:00:00Z', version: '0.1.0' }] },
    });
    render(<Pill connId="c-staging" />);
    const list = document.querySelector<HTMLElement>('[data-copies-into]')!;
    expect(within(list).getByText(ls('copyingInto', { label: 'copy a' }))).toBeInTheDocument();
    expect(document.querySelector('[data-instances-chip]')).toHaveTextContent(ws('otherInstances', { n: 1 }));
  });

  it('the connection switcher offers 새 창 and 데이터베이스 복사…', async () => {
    registerBuiltinActions();
    const openNew = vi.fn(async () => undefined);
    const open = vi.fn();
    useWindowsStore.setState({ openNew });
    useCopyStore.setState({ open });
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <WorkspaceToolbar connId="c-prod" />
      </TooltipProvider>,
    );
    await user.click(screen.getByRole('button', { name: new RegExp(ls('switchConnection')) }));
    await user.click(await screen.findByRole('menuitem', { name: new RegExp(ls('newWindow')) }));
    expect(openNew).toHaveBeenCalledWith();
    await user.click(screen.getByRole('button', { name: new RegExp(ls('switchConnection')) }));
    await user.click(await screen.findByRole('menuitem', { name: new RegExp(ls('copyDatabase')) }));
    // The source preset is this workspace's connection with its browse database and schema.
    expect(open).toHaveBeenCalledWith({ source: { connId: 'c-prod', database: 'orders', schema: 'public' } });
  });
});
