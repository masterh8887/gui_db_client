import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { exportStart } from '@/lib/commands';
import { stateLabel } from '@/lib/i18n';
import { mockBackend } from '@/lib/mock-ipc';
import { useExtractStore } from '@/stores/extract';
import { ALL_STATES, useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ActivityPanel } from './ActivityPanel';
import { chipTooltip } from './JobChip';
import { startLabel } from './JobRow';
import { jobTitle } from './JobTitle';
import { ms } from './strings';

function summary(state: JobState, id = `job-${state}`): JobSummary {
  return {
    id,
    kind: 'export',
    state,
    connId: 'c-prod',
    connName: 'prod (PostgreSQL)',
    label: `orders ${state}`,
    startedAt: '2026-10-26T10:00:00.000Z',
    ...(state === 'warned'
      ? {
          preflight: {
            warnings: [
              { code: 'WORK_MEM_HIGH', severity: 'warning', message: 'x' },
              { code: 'OVERWRITE', severity: 'warning', message: 'y' },
            ],
            blockers: [],
            estimates: { outputPath: 'x' },
            sessions: [],
            elapsedMs: BigInt(1),
          },
        }
      : {}),
    ...(state === 'done'
      ? { result: { rows: BigInt(2_000_000), bytes: BigInt(236_000_000), files: ['C:\\exports\\a.csv'], durationMs: BigInt(12_000), verifiedRows: BigInt(2_000_000), notes: ['csv verified'] } }
      : {}),
    ...(state === 'failed' ? { error: { code: 'IO', message: 'disk full', detail: 'No space left on device' } } : {}),
    ...(state === 'running'
      ? {
          progress: {
            jobId: id,
            rows: BigInt(500_000),
            bytesIn: BigInt(1),
            bytesOut: BigInt(59_000_000),
            batches: BigInt(8),
            files: 3,
            elapsedMs: BigInt(5_000),
            rateRowsPerSec: 100_000,
            rateBytesPerSec: 11_800_000,
            etaSec: BigInt(15),
            phase: 'transferring',
          },
          server: { jobId: id, pid: '4242', state: 'active', waitEvent: 'ClientWrite', bottleneck: 'network', tempBytes: BigInt(0) },
        }
      : {}),
  };
}

function renderPanel(variant: 'inspector' | 'popover' = 'inspector') {
  return render(
    <TooltipProvider>
      <ActivityPanel variant={variant} />
    </TooltipProvider>,
  );
}

describe('ActivityPanel', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.configure({ latencyMs: 0 });
  });

  it('shows the empty state without jobs', () => {
    renderPanel();
    expect(screen.getByText(ms('idle'))).toBeInTheDocument();
  });

  it.each(ALL_STATES)('renders a job in state %s with its label and state chip', (state) => {
    useJobsStore.getState().upsertSummary(summary(state));
    renderPanel();
    const row = screen.getByRole('article', { name: `orders ${state} ${stateLabel(state)}` });
    expect(within(row).getAllByText(stateLabel(state)).length).toBeGreaterThan(0);
    expect(row.querySelector(`[data-state="${state}"]`)).not.toBeNull();
  });

  it('shows progress, elapsed time, row rate, ETA and the server line for a running job', () => {
    useJobsStore.getState().upsertSummary(summary('running'));
    renderPanel('inspector');
    const row = screen.getByRole('article', { name: /orders running/ });
    expect(within(row).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '25');
    expect(within(row).getByText(ms('rows', { n: '500K' }))).toBeInTheDocument();
    expect(within(row).getByText('11.3 MB/s')).toBeInTheDocument();
    expect(within(row).getByText(ms('rowRate', { n: '100K' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('elapsed', { t: '0:05' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('eta', { t: '0:15' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('files', { n: 3 }))).toBeInTheDocument();
    expect(within(row).getByText(ms('server', { state: 'active' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('wait', { event: 'ClientWrite' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('bottleneck', { b: ms('bottleneckNetwork') }))).toBeInTheDocument();
    expect(within(row).getByText(ms('pid', { pid: '4242' }))).toBeInTheDocument();
    expect(within(row).getByRole('button', { name: ms('cancel') })).toBeInTheDocument();
  });

  it('leaves the server line out of the popover variant', () => {
    useJobsStore.getState().upsertSummary(summary('running'));
    renderPanel('popover');
    expect(screen.queryByText(ms('server', { state: 'active' }))).not.toBeInTheDocument();
    expect(screen.getByText(ms('elapsed', { t: '0:05' }))).toBeInTheDocument();
  });

  it('shows a finishing job with an indeterminate bar and no ETA', () => {
    const s = summary('running', 'job-fin');
    useJobsStore.getState().upsertSummary({ ...s, state: 'finishing', label: 'orders finishing' });
    renderPanel();
    const row = screen.getByRole('article', { name: /orders finishing/ });
    expect(within(row).getByRole('progressbar')).not.toHaveAttribute('aria-valuenow');
    expect(within(row).queryByText(ms('eta', { t: '0:15' }))).not.toBeInTheDocument();
  });

  it('lists active jobs newest first and the last five results with a clear button', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary({ ...summary('running', 'a'), label: 'first' });
    useJobsStore.getState().upsertSummary({ ...summary('running', 'b'), label: 'second' });
    for (let i = 0; i < 7; i += 1) useJobsStore.getState().upsertSummary({ ...summary('done', `d${i}`), label: `done ${i}` });
    renderPanel();
    const labels = screen.getAllByRole('article').map((a) => a.getAttribute('aria-label'));
    expect(labels.slice(0, 2)).toEqual([`second ${stateLabel('running')}`, `first ${stateLabel('running')}`]);
    expect(labels.filter((l) => l?.startsWith('done '))).toHaveLength(5);
    await user.click(screen.getByRole('button', { name: ms('clear') }));
    expect(screen.queryByText(/^done \d$/)).not.toBeInTheDocument();
    expect(screen.getAllByRole('article')).toHaveLength(2);
  });

  it('shows the result of a done job: rows, size, verification, files and notes', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(summary('done'));
    renderPanel();
    const row = screen.getByRole('article', { name: /orders done/ });
    expect(within(row).getByText(ms('rows', { n: '2,000,000' }))).toBeInTheDocument();
    expect(within(row).getByText(ms('verified', { n: '2,000,000' }))).toBeInTheDocument();
    expect(within(row).getByText('csv verified')).toBeInTheDocument();
    expect(within(row).getByText('a.csv')).toBeInTheDocument();
    await user.click(within(row).getByRole('button', { name: `${ms('openFolder')}: a.csv` }));
    await waitFor(() => expect(mockBackend.calls).toContain('open_path'));
  });

  it('shows the error strip with code, message and detail of a failed job', () => {
    useJobsStore.getState().upsertSummary(summary('failed'));
    renderPanel();
    const row = screen.getByRole('article', { name: /orders failed/ });
    // The shared JobErrorStrip: the same 12px mono code chip as the query tab's 작업 panel.
    expect(within(row).getByText('IO')).toHaveClass('font-mono', 'text-sm', 'h-[18px]');
    expect(within(row).getByText('disk full')).toBeInTheDocument();
    expect(within(row).getByText('No space left on device')).toHaveClass('whitespace-pre-wrap');
    expect(row.querySelector('.bg-danger-soft svg.lucide-circle-x')).not.toBeNull();
  });

  it('tints the error strip of a cancelled job as a warning', () => {
    useJobsStore.getState().upsertSummary({ ...summary('cancelled'), error: { code: 'CANCELLED', message: 'cancelled by user' } });
    renderPanel();
    const row = screen.getByRole('article', { name: /orders cancelled/ });
    expect(within(row).getByText('CANCELLED')).toBeInTheDocument();
    expect(row.querySelector('.bg-warning-soft svg.lucide-ban')).not.toBeNull();
    expect(row.querySelector('.bg-danger-soft')).toBeNull();
  });

  it('removes a finished job from the list', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(summary('cancelled'));
    renderPanel();
    await user.click(screen.getByRole('button', { name: ms('remove') }));
    expect(useJobsStore.getState().jobs['job-cancelled']).toBeUndefined();
  });

  it('shows the warning count of a warned job and opens the resume sheet on review', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(summary('warned'));
    renderPanel();
    expect(screen.getByText(ms('warned', { n: 2 }))).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: ms('review') }));
    await waitFor(() => expect(useExtractStore.getState().preflightDialog?.kind).toBe('resume'));
  });

  it('fetches a missing preflight report when the user reviews a warned job', async () => {
    const user = userEvent.setup();
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
    renderPanel();
    expect(screen.getByText(ms('warnedUnknown'))).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: ms('review') }));
    await waitFor(() => expect(useExtractStore.getState().preflightDialog?.kind).toBe('resume'));
    expect(mockBackend.calls).toContain('job_get');
    expect(useJobsStore.getState().jobs[jobId].summary.preflight?.warnings.map((w) => w.code)).toEqual(['OVERWRITE']);
  });

  it('asks before cancelling and calls job_cancel only after confirmation', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(summary('running'));
    renderPanel();
    await user.click(screen.getByRole('button', { name: ms('cancel') }));
    expect(useUiStore.getState().cancelConfirmJobId).toBe('job-running');
    const confirm = screen.getByRole('alertdialog', { name: ms('cancelConfirm') });
    // The keyboard lands on [아니요].
    expect(within(confirm).getByRole('button', { name: ms('no') })).toHaveFocus();
    await user.click(within(confirm).getByRole('button', { name: ms('no') }));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('job_cancel');

    await user.click(screen.getByRole('button', { name: ms('cancel') }));
    await user.click(screen.getByRole('button', { name: ms('yes') }));
    await waitFor(() => expect(mockBackend.calls).toContain('job_cancel'));
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    // The row keeps showing `running` until the backend says otherwise (job:state / job:done).
    expect(useJobsStore.getState().jobs['job-running'].summary.state).toBe('running');
  });

  it('dismisses the inline confirmation with Esc inside it', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(summary('running'));
    renderPanel();
    await user.click(screen.getByRole('button', { name: ms('cancel') }));
    fireEvent.keyDown(screen.getByRole('button', { name: ms('no') }), { key: 'Escape' });
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('shows the confirmation asked for by the global shortcut and drops it once the job ends', () => {
    useJobsStore.getState().upsertSummary(summary('running'));
    renderPanel();
    act(() => useUiStore.getState().setCancelConfirm('job-running'));
    expect(screen.getByRole('alertdialog')).toBeInTheDocument();
    act(() => useJobsStore.getState().applyDone({ jobId: 'job-running', state: 'cancelled' }));
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('never shows a cancel confirmation on a finished job', () => {
    useJobsStore.getState().upsertSummary(summary('done'));
    useUiStore.getState().setCancelConfirm('job-done');
    renderPanel();
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });
});

describe('startLabel', () => {
  it('shows the time for today and the date for other days', () => {
    const now = new Date(2026, 9, 27, 15, 0);
    expect(startLabel(new Date(2026, 9, 27, 9, 5).toISOString(), now)).toBe('09:05');
    expect(startLabel(new Date(2026, 9, 26, 23, 59).toISOString(), now)).toBe('10-26 23:59');
    expect(startLabel('garbage', now)).toBe('garbage');
  });
});

describe('copy job title', () => {
  beforeEach(() => resetStores());

  const copyJob: JobSummary = {
    id: 'job-copy',
    kind: 'copy',
    state: 'running',
    connId: 'c-legacy',
    connName: 'legacy (MySQL 4.0, EUC-KR)',
    label: 'legacy.shop → mysql8.shop',
    startedAt: '2026-10-27T10:00:00.000Z',
    copy: { runId: 'job-copy', targetConnId: 'c-mysql8', targetConnName: 'mysql8 (MySQL 8.0)', sourceDatabase: 'shop', targetDatabase: 'shop_utf8', phase: 'data', tablesTotal: 8, tablesDone: 3, tablesFailed: 0, dryRun: false },
  };

  it('reads "복사: {src}.{db} → {tgt}.{db}" with the Copy icon; the source end truncates first, the full text is the tooltip', () => {
    useJobsStore.getState().upsertSummary(copyJob);
    render(
      <TooltipProvider>
        <ActivityPanel variant="popover" />
      </TooltipProvider>,
    );
    const full = ms('copyTitle', { source: 'legacy (MySQL 4.0, EUC-KR).shop', target: 'mysql8 (MySQL 8.0).shop_utf8' });
    expect(full).toBe('복사: legacy (MySQL 4.0, EUC-KR).shop → mysql8 (MySQL 8.0).shop_utf8');
    const row = screen.getByRole('article', { name: `${full} ${stateLabel('running')}` });
    const title = within(row).getByTestId('copy-job-title');
    expect(title).toHaveAttribute('title', full);
    const [prefix, source, arrow, target] = [...title.children];
    expect(prefix).toHaveTextContent(ms('copyPrefix'));
    expect(arrow).toHaveTextContent('→');
    // Middle truncation: the ends share a short line 2 : 3, the connection names truncate and
    // each `.database` stays visible.
    expect(source).toHaveAttribute('data-end', 'source');
    expect(source).toHaveTextContent('legacy (MySQL 4.0, EUC-KR).shop');
    expect(source).toHaveClass('flex-[2_1_0%]', 'max-w-max', 'overflow-hidden');
    expect(target).toHaveAttribute('data-end', 'target');
    expect(target).toHaveTextContent('mysql8 (MySQL 8.0).shop_utf8');
    expect(target).toHaveClass('flex-[3_1_0%]', 'max-w-max', 'overflow-hidden');
    for (const end of [source, target]) {
      expect(end.children[0]).toHaveClass('truncate');
      expect(end.children[1]).toHaveClass('shrink-0');
    }
    expect(target.children[1]).toHaveTextContent('.shop_utf8');
    expect(within(row).getByTitle(ms('kindCopy'))).toBeInTheDocument();
    expect(jobTitle(copyJob)).toBe(full);
    expect(chipTooltip({ summary: copyJob, logs: [], updatedAt: 0 })).toContain(full);
    // Other jobs keep their label; a copy stub without `copy` too.
    expect(jobTitle({ ...copyJob, kind: 'export' })).toBe(copyJob.label);
    expect(jobTitle({ ...copyJob, copy: undefined })).toBe(copyJob.label);
  });
});
