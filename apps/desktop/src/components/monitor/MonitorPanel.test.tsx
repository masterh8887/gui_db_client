import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { exportStart } from '@/lib/commands';
import { stateLabel, t } from '@/lib/i18n';
import { mockBackend } from '@/lib/mock-ipc';
import { useExtractStore } from '@/stores/extract';
import { ALL_STATES, useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { MonitorPanel } from './MonitorPanel';

function summary(state: JobState): JobSummary {
  return {
    id: `job-${state}`,
    kind: 'export',
    state,
    connId: 'c-prod',
    connName: 'prod (PostgreSQL)',
    label: `orders ${state}`,
    startedAt: '2026-10-26T10:00:00.000Z',
    ...(state === 'warned'
      ? { preflight: { warnings: [{ code: 'WORK_MEM_HIGH', severity: 'warning', message: 'x' }], blockers: [], estimates: { outputPath: 'x' }, sessions: [], elapsedMs: BigInt(1) } }
      : {}),
    ...(state === 'done' ? { result: { rows: BigInt(2_000_000), bytes: BigInt(236_000_000), files: ['C:\\exports\\a.csv'], durationMs: BigInt(12_000), notes: [] } } : {}),
    ...(state === 'failed' ? { error: { code: 'IO', message: 'disk full' } } : {}),
    ...(state === 'running'
      ? {
          progress: {
            jobId: 'job-running',
            rows: BigInt(500_000),
            bytesIn: BigInt(1),
            bytesOut: BigInt(59_000_000),
            batches: BigInt(8),
            files: 1,
            elapsedMs: BigInt(5_000),
            rateRowsPerSec: 100_000,
            rateBytesPerSec: 11_800_000,
            etaSec: BigInt(15),
            phase: 'transferring',
          },
        }
      : {}),
  };
}

describe('MonitorPanel', () => {
  beforeEach(() => resetStores());

  it.each(ALL_STATES)('renders a job in state %s with its label', (state) => {
    useJobsStore.getState().upsertSummary(summary(state));
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    expect(screen.getAllByText(stateLabel(state)).length).toBeGreaterThan(0);
    expect(screen.getByText(`orders ${state}`)).toBeInTheDocument();
  });

  it('shows progress numbers and a cancel control for a running job', () => {
    useJobsStore.getState().upsertSummary(summary('running'));
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    expect(screen.getByText('500,000')).toBeInTheDocument();
    expect(screen.getByText('11.3 MB/s')).toBeInTheDocument();
    expect(screen.getByText('~15s')).toBeInTheDocument();
    expect(screen.getByRole('progressbar')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /취소|Cancel/ })).toBeInTheDocument();
  });

  it('offers the acknowledge button for a warned job and the files for a done job', () => {
    useJobsStore.getState().upsertSummary(summary('warned'));
    useJobsStore.getState().upsertSummary(summary('done'));
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    expect(screen.getByRole('button', { name: /경고 확인|Review warnings/ })).toBeEnabled();
    expect(screen.getByText('a.csv')).toBeInTheDocument();
    expect(screen.getByText('2,000,000')).toBeInTheDocument();
  });

  it('fetches a missing preflight report when the user reviews a warned job', async () => {
    const user = userEvent.setup();
    mockBackend.configure({ latencyMs: 0 });
    const jobId = await exportStart(
      {
        connId: 'c-prod',
        sql: 'SELECT * FROM public.orders',
        session: {},
        output: { dir: 'C:/exports', namePattern: '{conn}_{name}', format: 'csv', overwrite: true },
      },
      [],
    );
    await waitFor(() => expect(mockBackend.getJob(jobId)?.state).toBe('warned'), { timeout: 3000 });
    // As if the `job_get` answer after `job:state warned` never arrived: no report yet.
    useJobsStore.getState().upsertSummary({ ...mockBackend.getJob(jobId)!, preflight: undefined });
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    const review = screen.getByRole('button', { name: t('monitor.acknowledge') });
    expect(review).toBeEnabled();
    await user.click(review);
    await waitFor(() => expect(useExtractStore.getState().preflightDialog?.kind).toBe('resume'));
    expect(mockBackend.calls).toContain('job_get');
    expect(useJobsStore.getState().jobs[jobId].summary.preflight?.warnings.map((w) => w.code)).toEqual(['OVERWRITE']);
  });

  it('asks before cancelling and calls job_cancel only after confirmation', async () => {
    const user = userEvent.setup();
    mockBackend.configure({ latencyMs: 0 });
    useJobsStore.getState().upsertSummary(summary('running'));
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    await user.click(screen.getByRole('button', { name: t('monitor.cancel') }));
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: t('common.no') }));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('job_cancel');

    await user.click(screen.getByRole('button', { name: t('monitor.cancel') }));
    await user.click(screen.getByRole('button', { name: t('common.yes') }));
    await waitFor(() => expect(mockBackend.calls).toContain('job_cancel'));
    // The card keeps showing `running` until the backend says otherwise (job:state / job:done).
    expect(useJobsStore.getState().jobs['job-running'].summary.state).toBe('running');
  });

  it('never shows a cancel confirmation on a finished job', () => {
    useJobsStore.getState().upsertSummary(summary('done'));
    useUiStore.getState().setCancelConfirm('job-done');
    render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });
});
