import type { ExportRequest } from 'ipc-types';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { catalogExactCount, exportStart, jobGet } from '@/lib/commands';
import { stateLabel, t } from '@/lib/i18n';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { useSettingsStore } from '@/stores/settings';
import { resetStores } from '@/test/stores';
import { tableKey } from '@/lib/utils';
import { MonitorPanel } from '@/components/monitor/MonitorPanel';
import { TooltipProvider } from '@/components/ui';
import { AppEvents } from './AppEvents';

// The real commands (over the mock backend), with `jobGet` wrapped so a test can hold one call.
vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, jobGet: vi.fn(actual.jobGet) };
});

const req: ExportRequest = {
  connId: 'c-prod',
  sql: 'SELECT * FROM public.orders',
  session: { workMem: '64MB' },
  output: { dir: 'C:/exports', namePattern: '{conn}_{name}_reattach', format: 'csv', overwrite: false },
};

describe('AppEvents', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, progressTickMs: 10, prepareMs: 20, runMs: 250, finishingMs: 10, serverIntervalMs: 50, exactCountMs: 30 });
  });

  it('bootstraps the stores and re-attaches a job that was already running (job_list)', async () => {
    // Started before the UI mounted, as after a window reload: its earlier events were missed.
    const jobId = await exportStart(req, []);
    render(<AppEvents />);

    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]).toBeDefined());
    expect(mockBackend.calls).toContain('job_list');
    expect(useConnectionsStore.getState().loaded).toBe(true);
    await waitFor(() => expect(useSettingsStore.getState().settings).not.toBeNull());

    // From here on only events move it; the store never advances a job by itself.
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId].summary.state).toBe('done'), { timeout: 3000 });
    const entry = useJobsStore.getState().jobs[jobId];
    expect(entry.summary.result?.files[0]).toMatch(/reattach\.csv$/);
    expect(entry.logs.length).toBeGreaterThan(0);
    await waitFor(() => expect(useHistoryStore.getState().entries.some((h) => h.jobId === jobId)).toBe(true));
  });

  it('fetches the preflight report when a known job enters warned so it can be acknowledged', async () => {
    const user = userEvent.setup();
    render(<AppEvents />);
    await waitFor(() => expect(useJobsStore.getState().loaded).toBe(true));
    // A start whose preflight (run again by the backend) finds a warning nobody acknowledged:
    // the job is known from its `pending` event long before `warned` arrives.
    const jobId = await exportStart({ ...req, output: { ...req.output, overwrite: true } }, []);
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.state).toBe('warned'), { timeout: 3000 });
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId].summary.preflight?.warnings.map((w) => w.code)).toEqual(['OVERWRITE']));

    // Mounted only while the job waits (no events flow then), so no update escapes act().
    const panel = render(
      <TooltipProvider>
        <MonitorPanel />
      </TooltipProvider>,
    );
    const card = screen.getByRole('article', { name: new RegExp(stateLabel('warned')) });
    expect(within(card).getByText(`1 ${t('extract.warnings')}`)).toBeInTheDocument();
    const review = within(card).getByRole('button', { name: t('monitor.acknowledge') });
    expect(review).toBeEnabled();
    await user.click(review);
    const dialog = useExtractStore.getState().preflightDialog;
    expect(dialog?.kind).toBe('resume');
    expect(dialog?.report.warnings[0].code).toBe('OVERWRITE');
    panel.unmount();

    // Acknowledging resumes the job; from there only events move it.
    await useExtractStore.getState().startAcknowledged(['OVERWRITE']);
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId].summary.state).toBe('done'), { timeout: 3000 });
  });

  it('routes an exact count result into the catalog and clears the counting flag', async () => {
    render(<AppEvents />);
    await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
    const ref = { database: 'orders', schema: 'public', name: 'orders' };
    const jobId = await catalogExactCount('c-prod', ref);
    useCatalogStore.getState().setCounting('c-prod', ref, jobId);

    const key = tableKey('c-prod', ref);
    await waitFor(() => expect(useCatalogStore.getState().exactCounts[key]).toBeDefined(), { timeout: 3000 });
    expect(useCatalogStore.getState().counting[key]).toBeUndefined();
    expect(useJobsStore.getState().jobs[jobId].summary.kind).toBe('exact_count');
  });

  it('routes a count whose job:done arrives while the job is still a job:state stub', async () => {
    render(<AppEvents />);
    await waitFor(() => expect(useJobsStore.getState().loaded).toBe(true));
    // The `job_get` that fills in the stub created by the first `job:state` stays in flight
    // until the end of the test, so `job:done` finds an entry without a connection.
    const actual = await vi.importActual<typeof import('@/lib/commands')>('@/lib/commands');
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    vi.mocked(jobGet).mockImplementationOnce(async (id) => {
      await gate;
      return actual.jobGet(id);
    });
    const ref = { database: 'orders', schema: 'public', name: 'orders' };
    const jobId = await catalogExactCount('c-prod', ref);
    useCatalogStore.getState().setCounting('c-prod', ref, jobId);

    const key = tableKey('c-prod', ref);
    await waitFor(() => expect(useCatalogStore.getState().exactCounts[key]).toBeDefined(), { timeout: 3000 });
    expect(useCatalogStore.getState().counting[key]).toBeUndefined();
    expect(useJobsStore.getState().jobs[jobId].summary).toMatchObject({ kind: 'exact_count', connId: 'c-prod', state: 'done' });
    // The stub's own fetch is still held: the count got its connection from a fetch of its own.
    expect(vi.mocked(jobGet)).toHaveBeenCalledWith(jobId);
    release();
  });

  it('clears the counting flag when a count job ends without a count', async () => {
    render(<AppEvents />);
    await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
    const ref = { database: 'orders', schema: 'public', name: 'orders' };
    useCatalogStore.getState().setCounting('c-prod', ref, 'cnt-gone');
    mockBackend.emit('job:done', { jobId: 'cnt-gone', state: 'failed', error: { code: 'QUERY', message: 'canceling statement due to statement timeout' } });
    await waitFor(() => expect(useCatalogStore.getState().counting[tableKey('c-prod', ref)]).toBeUndefined());
  });
});
