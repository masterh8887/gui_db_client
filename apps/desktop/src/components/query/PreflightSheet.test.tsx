import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ExportRequest, JobSummary, PreflightIssue, PreflightReport } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { stateLabel } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { defaultOutputOpts, useExtractStore } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { resetStores } from '@/test/stores';
import { PreflightSheet } from './PreflightSheet';
import { qs } from './strings';

const warning = (code: string): PreflightIssue => ({ code, severity: 'warning', message: `${code} message`, detail: `${code} detail`, recommendation: `${code} fix` });

function report(over: Partial<PreflightReport> = {}): PreflightReport {
  return {
    warnings: [warning('WORK_MEM_HIGH'), warning('CONCURRENT_SESSION')],
    blockers: [],
    estimates: {
      rows: BigInt(48_213_000),
      bytes: BigInt(6_100_000_000),
      outputPath: 'C:\\exports\\prod_orders.csv',
      files: 3,
      diskFreeBytes: BigInt(200_000_000_000),
      workMem: '512MB',
      recommendedWorkMem: '128MB',
      serverRamBytes: BigInt(8 * 1024 ** 3),
    },
    sessions: [
      { pid: '4242', user: 'etl', application: 'loader', state: 'idle in transaction', waitEvent: 'ClientRead', elapsedMs: BigInt(600_000), blocking: true, query: 'UPDATE orders SET x = 1', tables: ['public.orders'] },
      { pid: '4343', user: 'app', application: 'web', state: 'active', elapsedMs: BigInt(1_000), blocking: false, tables: [] },
    ],
    elapsedMs: BigInt(31),
    ...over,
  };
}

const req = (): ExportRequest => ({ connId: 'c-prod', sql: 'SELECT * FROM public.orders', session: {}, output: { ...defaultOutputOpts(), dir: 'C:\\exports' } });

function job(id: string, state: JobSummary['state']): JobSummary {
  return { id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: 'orders', startedAt: '2026-10-27T10:00:00Z' };
}

function openRun(r: PreflightReport = report()) {
  act(() => useExtractStore.setState({ preflightDialog: { kind: 'run', tabId: 'q1', req: req(), report: r, busy: false, error: null } }));
}

function renderSheet() {
  return render(
    <TooltipProvider>
      <PreflightSheet />
    </TooltipProvider>,
  );
}

const sheet = () => screen.getByRole('dialog');
const ack = (code: string) => within(sheet()).getByRole('checkbox', { name: qs('acknowledgeAria', { code }) });

describe('PreflightSheet', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('renders nothing without a report', () => {
    renderSheet();
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('shows the connection tag, counts, warnings, estimates and sessions', () => {
    renderSheet();
    openRun();
    const s = sheet();
    expect(within(s).getByText(qs('preflightTitle'))).toBeInTheDocument();
    expect(within(s).getByText('production')).toBeInTheDocument();
    expect(within(s).getByText(qs('preflightCounts', { b: 0, w: 2 }))).toBeInTheDocument();
    expect(within(s).getByText('WORK_MEM_HIGH fix')).toBeInTheDocument();
    // Row estimates are labelled as estimates; the output path shows the extra files.
    expect(within(s).getByText(/≈ 48,213,000/)).toBeInTheDocument();
    expect(within(s).getByText('(+2)')).toBeInTheDocument();
    expect(within(s).getByText('128MB')).toBeInTheDocument();
    const blocking = s.querySelector('tr[data-blocking]');
    expect(blocking).toHaveTextContent('4242');
    expect(s.querySelectorAll('tbody tr')).toHaveLength(2);
  });

  it('enables 시작 only once every warning is acknowledged and starts with their codes', async () => {
    const user = userEvent.setup();
    const start = vi.spyOn(useExtractStore.getState(), 'startAcknowledged').mockResolvedValue('job-9');
    renderSheet();
    openRun();
    const button = within(sheet()).getByRole('button', { name: qs('start') });
    expect(button).toBeDisabled();
    await user.click(ack('WORK_MEM_HIGH'));
    expect(button).toBeDisabled();
    await user.click(ack('CONCURRENT_SESSION'));
    expect(button).toBeEnabled();
    await user.click(button);
    expect(start).toHaveBeenCalledWith(['WORK_MEM_HIGH', 'CONCURRENT_SESSION']);
  });

  it('keeps 시작 disabled while a blocker exists, with the reason', () => {
    renderSheet();
    openRun(report({ warnings: [], blockers: [{ code: 'DISK_LOW', severity: 'danger', message: 'not enough disk' }] }));
    const s = sheet();
    expect(within(s).getByText('DISK_LOW')).toBeInTheDocument();
    expect(within(s).queryByRole('checkbox')).toBeNull();
    expect(within(s).getByRole('button', { name: qs('start') })).toBeDisabled();
    expect(within(s).getByTitle(qs('blocked'))).toBeInTheDocument();
  });

  it('closes on 취소 and on Esc, but not while starting', async () => {
    const user = userEvent.setup();
    renderSheet();
    openRun();
    await user.click(within(sheet()).getByRole('button', { name: qs('cancel') }));
    expect(useExtractStore.getState().preflightDialog).toBeNull();

    openRun();
    await user.keyboard('{Escape}');
    expect(useExtractStore.getState().preflightDialog).toBeNull();

    act(() => useExtractStore.setState({ preflightDialog: { kind: 'run', tabId: 'q1', req: req(), report: report(), busy: true, error: null } }));
    expect(within(sheet()).getByRole('button', { name: qs('cancel') })).toBeDisabled();
    await user.keyboard('{Escape}');
    expect(useExtractStore.getState().preflightDialog).not.toBeNull();
  });

  it('shows the start error on top', () => {
    renderSheet();
    act(() => useExtractStore.setState({ preflightDialog: { kind: 'run', tabId: 'q1', req: req(), report: report(), busy: false, error: 'boom' } }));
    expect(within(sheet()).getByText('boom')).toBeInTheDocument();
  });

  it('shows queued sheets one after another, each with fresh acknowledgements', async () => {
    const user = userEvent.setup();
    renderSheet();
    act(() => {
      useJobsStore.getState().upsertSummary(job('job-1', 'warned'));
      useExtractStore.getState().openResumeDialog('job-1', report({ warnings: [warning('WORK_MEM_HIGH')] }));
      useExtractStore.getState().openResumeDialog('job-1', report({ warnings: [warning('WORK_MEM_HIGH')], elapsedMs: BigInt(77) }));
    });
    expect(within(sheet()).getByText(qs('queued', { n: 1 }), { exact: false })).toBeInTheDocument();
    await user.click(ack('WORK_MEM_HIGH'));
    expect(within(sheet()).getByRole('button', { name: qs('resume') })).toBeEnabled();
    await user.click(within(sheet()).getByRole('button', { name: qs('cancel') }));
    // The next report is shown with nothing acknowledged.
    await waitFor(() => expect(useExtractStore.getState().preflightQueue).toHaveLength(0));
    expect(ack('WORK_MEM_HIGH')).not.toBeChecked();
    expect(within(sheet()).getByRole('button', { name: qs('resume') })).toBeDisabled();
  });

  it('resume: disables 계속 once the job leaves warned, leaving only 닫기', async () => {
    const user = userEvent.setup();
    renderSheet();
    act(() => {
      useJobsStore.getState().upsertSummary(job('job-1', 'warned'));
      useExtractStore.getState().openResumeDialog('job-1', report({ warnings: [warning('WORK_MEM_HIGH')] }));
    });
    await user.click(ack('WORK_MEM_HIGH'));
    expect(within(sheet()).getByRole('button', { name: qs('resume') })).toBeEnabled();

    act(() => useJobsStore.getState().applyState({ jobId: 'job-1', state: 'cancelled', at: '2026-10-27T10:05:00Z' }));
    expect(within(sheet()).getByText(qs('stateChanged', { state: stateLabel('cancelled') }))).toBeInTheDocument();
    expect(within(sheet()).queryByRole('button', { name: qs('resume') })).toBeNull();
    await user.click(within(sheet()).getByRole('button', { name: qs('close') }));
    expect(useExtractStore.getState().preflightDialog).toBeNull();
  });
});
