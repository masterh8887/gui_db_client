import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { askCancel } from '@/lib/job-actions';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { JobChip } from './JobChip';
import { ms } from './strings';

function job(id: string, state: JobState, extra: Partial<JobSummary> = {}): JobSummary {
  return { id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: `label ${id}`, startedAt: '2026-10-27T10:00:00.000Z', ...extra };
}

const running = (id: string, eta = 30) =>
  job(id, 'running', {
    progress: {
      jobId: id,
      rows: BigInt(1_000_000),
      bytesIn: BigInt(0),
      bytesOut: BigInt(0),
      batches: BigInt(1),
      files: 1,
      elapsedMs: BigInt(10_000),
      rateRowsPerSec: 1,
      rateBytesPerSec: 1,
      etaSec: BigInt(eta),
      phase: 'transferring',
    },
  });

function renderChip(variant: 'toolbar' | 'welcome' = 'toolbar') {
  return render(
    <TooltipProvider>
      <button type="button">before</button>
      <JobChip variant={variant} />
    </TooltipProvider>,
  );
}

const chip = () => screen.getByRole('button', { name: new RegExp(`^${ms('chipTip')}`) });

describe('JobChip', () => {
  beforeEach(() => resetStores());

  it('renders nothing while the jobs store is empty', () => {
    renderChip();
    expect(screen.queryByRole('button', { name: new RegExp(`^${ms('chipTip')}`) })).not.toBeInTheDocument();
  });

  it('shows the primary running job percent and +N for the other active jobs', () => {
    useJobsStore.getState().upsertSummary(running('a'));
    useJobsStore.getState().upsertSummary(job('b', 'pending'));
    renderChip();
    // 10s elapsed + 30s left = 25%.
    expect(within(chip()).getByText('25%')).toBeInTheDocument();
    expect(within(chip()).getByText(ms('chipMore', { n: 1 }))).toBeInTheDocument();
    expect(chip()).toHaveAttribute('data-job-state', 'running');
    expect(chip().getAttribute('aria-label')).toContain(ms('rows', { n: '1.0M' }));
    expect(chip().getAttribute('aria-label')).toContain(ms('eta', { t: '0:30' }));
  });

  it('puts a warned job first with the warning text', () => {
    useJobsStore.getState().upsertSummary(running('a'));
    useJobsStore.getState().upsertSummary(job('w', 'warned'));
    renderChip();
    expect(within(chip()).getByText(ms('chipWarned'))).toBeInTheDocument();
    expect(chip()).toHaveAttribute('data-job-state', 'warned');
  });

  it('shows only a quiet state icon when every job has finished', () => {
    useJobsStore.getState().upsertSummary(job('d', 'done'));
    renderChip();
    expect(chip()).toHaveAttribute('data-job-state', 'done');
    expect(chip().textContent).toBe('');
  });

  it('follows job:* events: the chip changes only when the store does', () => {
    useJobsStore.getState().upsertSummary(running('a'));
    renderChip();
    expect(chip()).toHaveAttribute('data-job-state', 'running');
    act(() => useJobsStore.getState().applyDone({ jobId: 'a', state: 'failed', error: { code: 'IO', message: 'x' } }));
    expect(chip()).toHaveAttribute('data-job-state', 'failed');
  });

  it('opens the inspector on 활동 in a workspace without moving focus', async () => {
    const user = userEvent.setup();
    useWorkspaceStore.setState({ mode: 'workspace', openConnId: 'c-prod' });
    useUiStore.setState({ inspectorOpen: false, inspectorSegment: 'details' });
    useJobsStore.getState().upsertSummary(running('a'));
    renderChip('toolbar');
    await user.click(chip());
    expect(useUiStore.getState().inspectorOpen).toBe(true);
    expect(useUiStore.getState().inspectorSegment).toBe('activity');
  });

  it('opens the activity popover on Welcome, which takes no focus, and closing it clears a cancel confirmation', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(running('a'));
    renderChip('welcome');
    await user.click(chip());
    const popover = screen.getByRole('dialog', { name: ms('popover') });
    expect(within(popover).getByRole('article', { name: /label a/ })).toBeInTheDocument();
    expect(popover.contains(document.activeElement)).toBe(false);

    act(() => askCancel('a'));
    expect(useUiStore.getState().cancelConfirmJobId).toBe('a');
    await user.click(screen.getByRole('button', { name: 'before' }));
    expect(screen.queryByRole('dialog', { name: ms('popover') })).not.toBeInTheDocument();
    expect(useUiStore.getState().activityPopoverOpen).toBe(false);
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
  });

  it('opens the popover from showActivity (Esc on Welcome) and toggles it closed on a second click', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(running('a'));
    renderChip('welcome');
    act(() => askCancel('a'));
    expect(screen.getByRole('dialog', { name: ms('popover') })).toBeInTheDocument();
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
    await user.click(chip());
    expect(useUiStore.getState().activityPopoverOpen).toBe(false);
  });

  it('clears the popover flag when [지우기] empties the store, so the next job does not open it by itself', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(job('a', 'done'));
    useJobsStore.getState().upsertSummary(job('b', 'failed', { error: { code: 'IO', message: 'disk full' } }));
    renderChip('welcome');
    await user.click(chip());
    const popover = screen.getByRole('dialog', { name: ms('popover') });
    await user.click(within(popover).getByRole('button', { name: ms('clear') }));

    expect(screen.queryByRole('dialog', { name: ms('popover') })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: new RegExp(`^${ms('chipTip')}`) })).not.toBeInTheDocument();
    expect(useUiStore.getState().activityPopoverOpen).toBe(false);

    act(() => useJobsStore.getState().upsertSummary(running('c')));
    expect(chip()).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByRole('dialog', { name: ms('popover') })).not.toBeInTheDocument();
    // showActivity() opens it again (it is no longer taken as already open).
    await user.click(chip());
    expect(screen.getByRole('dialog', { name: ms('popover') })).toBeInTheDocument();
  });

  it('clears the popover flag and its cancel confirmation when Welcome unmounts with the popover open', () => {
    useJobsStore.getState().upsertSummary(running('a'));
    const { unmount } = renderChip('welcome');
    act(() => askCancel('a'));
    expect(useUiStore.getState().activityPopoverOpen).toBe(true);
    unmount();
    expect(useUiStore.getState().activityPopoverOpen).toBe(false);
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
  });

  it('leaves a cancel confirmation alone when the toolbar chip unmounts', () => {
    useWorkspaceStore.setState({ mode: 'workspace', openConnId: 'c-prod' });
    useJobsStore.getState().upsertSummary(running('a'));
    const { unmount } = renderChip('toolbar');
    act(() => askCancel('a'));
    unmount();
    expect(useUiStore.getState().cancelConfirmJobId).toBe('a');
    expect(useUiStore.getState().activityPopoverOpen).toBe(false);
  });
});
