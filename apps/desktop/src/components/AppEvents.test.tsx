import type { ExportRequest } from 'ipc-types';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { catalogExactCount, exportStart, jobGet } from '@/lib/commands';
import { registerBuiltinActions } from '@/lib/actions';
import { setConfirmHandler } from '@/lib/confirm';
import { initWindows, resetWindowsInit, wins } from '@/stores/windows';
import { stateLabel } from '@/lib/i18n';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';
import { useToastStore } from '@/lib/toast';
import { resetStores } from '@/test/stores';
import { tableKey } from '@/lib/utils';
import { ActivityPanel } from '@/components/monitor/ActivityPanel';
import { ms } from '@/components/monitor/strings';
import { TooltipProvider } from '@/components/ui';
import { AppEvents, menuActionId } from './AppEvents';

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
    // A history page some panel already loaded (sidebar 기록, palette): reloaded after the export.
    await useHistoryStore.getState().load('c-prod');
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
    await waitFor(() => expect(useHistoryStore.getState().lists['c-prod'].entries.some((h) => h.jobId === jobId)).toBe(true));
    // No legacy output patch at bootstrap: no query document was created behind the UI's back.
    expect(Object.keys(useExtractStore.getState().docs)).toEqual([]);
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
        <ActivityPanel variant="inspector" />
      </TooltipProvider>,
    );
    const card = screen.getByRole('article', { name: new RegExp(stateLabel('warned')) });
    expect(within(card).getByText(ms('warned', { n: 1 }))).toBeInTheDocument();
    const review = within(card).getByRole('button', { name: ms('review') });
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

  it('toasts a local copy/export completion, and stays silent for a job another window owns', async () => {
    render(<AppEvents />);
    await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
    const now = new Date().toISOString();
    // A copy job this window started (claimed): its completion toast shows here (db-copy.md 1.1).
    const mine = 'cp-mine';
    useJobsStore.getState().upsertSummary({ id: mine, kind: 'copy', state: 'running', connId: 'c-prod', connName: 'prod', label: 'prod → mysql8', startedAt: now });
    useJobsStore.getState().addLocal([mine]);
    mockBackend.emit('job:done', { jobId: mine, state: 'done' });
    await waitFor(() => expect(useToastStore.getState().toasts.some((t) => t.message.includes('prod → mysql8'))).toBe(true));

    // A job another window owns (not local) raises no toast in this window.
    const other = 'cp-other';
    useJobsStore.getState().upsertSummary({ id: other, kind: 'copy', state: 'running', connId: 'c-prod', connName: 'prod', label: 'owned-elsewhere', startedAt: now });
    mockBackend.emit('job:done', { jobId: other, state: 'done' });
    await new Promise((r) => setTimeout(r, 0));
    expect(useToastStore.getState().toasts.some((t) => t.message.includes('owned-elsewhere'))).toBe(false);
  });

  it('coalesces a burst of connections:changed into a single reload', async () => {
    render(<AppEvents />);
    await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
    const connListCalls = () => mockBackend.calls.filter((c) => c === 'conn_list').length;
    const before = connListCalls();
    mockBackend.emit('connections:changed', null);
    mockBackend.emit('connections:changed', null);
    mockBackend.emit('connections:changed', null);
    await waitFor(() => expect(connListCalls()).toBe(before + 1), { timeout: 1000 });
    // No further reload escapes the coalescing window.
    await new Promise((r) => setTimeout(r, 150));
    expect(connListCalls()).toBe(before + 1);
  });

  it('routes macOS menu items (menu:action) to runAction; disabled and unknown ids are no-ops', async () => {
    registerBuiltinActions();
    render(<AppEvents />);
    await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
    mockBackend.emit('menu:action', 'app.settings');
    await waitFor(() => expect(useUiStore.getState().settingsOpen).toBe(true));
    mockBackend.emit('menu:action', { id: 'app.about' });
    await waitFor(() => expect(useUiStore.getState().aboutOpen).toBe(true));
    // tabs.close is disabled on Welcome; an unknown id only logs.
    mockBackend.emit('menu:action', 'tabs.close');
    mockBackend.emit('menu:action', 'no.such.action');
    await waitFor(() => expect(useUiStore.getState().palette.open).toBe(false));
  });

  it('leaves app:quit-requested to the windows store: one quit question with AppEvents mounted', async () => {
    const asked: string[] = [];
    setConfirmHandler((o) => {
      asked.push(o.body ?? '');
      return Promise.resolve(false);
    });
    try {
      render(<AppEvents />);
      await initWindows();
      await waitFor(() => expect(useConnectionsStore.getState().loaded).toBe(true));
      mockBackend.emitTo('main', 'app:quit-requested', { jobs: 1 });
      await waitFor(() => expect(asked).toHaveLength(1));
      await new Promise((r) => setTimeout(r, 0));
      expect(asked).toEqual([wins('quitBody', { n: 1 })]);
      expect(mockBackend.calls).not.toContain('app_quit_confirmed');
    } finally {
      setConfirmHandler(null);
      resetWindowsInit();
    }
  });
});

describe('menuActionId', () => {
  it('reads a plain id or an object payload', () => {
    expect(menuActionId('tabs.close')).toBe('tabs.close');
    expect(menuActionId({ id: 'query.run' })).toBe('query.run');
    expect(menuActionId({ action: 'app.about' })).toBe('app.about');
    expect(menuActionId('')).toBeNull();
    expect(menuActionId(42)).toBeNull();
    expect(menuActionId(null)).toBeNull();
  });
});
