import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { JobState, JobSummary, PreflightReport } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore, type QueryDocInit } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { PreflightSheet } from './PreflightSheet';
import { outputSummary } from './QueryBar';
import { clampPanelHeight, QueryTabView } from './QueryTabView';
import { qs } from './strings';

const ORDERS = 'SELECT * FROM public.orders';
const doc = (tabId: string) => useExtractStore.getState().docs[tabId];

function openTab(init?: QueryDocInit, connId = 'c-prod'): string {
  return useTabsStore.getState().openQuery(connId, init);
}

function renderTab(tabId: string) {
  return render(
    <TooltipProvider>
      <QueryTabView tabId={tabId} />
      <PreflightSheet />
    </TooltipProvider>,
  );
}

function setup(init?: QueryDocInit, connId = 'c-prod') {
  const tabId = openTab(init, connId);
  const view = renderTab(tabId);
  return { tabId, ...view };
}

const button = (name: string) => screen.getByRole('button', { name });
const segment = (name: string) => within(screen.getByRole('tablist', { name: qs('panelSections') })).getByRole('tab', { name: new RegExp(`^${name}`) });
const panel = () => screen.queryByRole('region', { name: qs('panelSections') });

function job(id: string, state: JobState, extra: Partial<JobSummary> = {}): JobSummary {
  return { id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: 'orders', startedAt: '2026-10-27T10:00:00Z', ...extra };
}

/** Bind a job to the tab as its last export, the way a successful `export_start` does. */
function attachJob(tabId: string, jobId: string) {
  act(() => useExtractStore.setState((s) => ({ docs: { ...s.docs, [tabId]: { ...s.docs[tabId], lastJobId: jobId } } })));
}

const warnedReport = (): PreflightReport => ({
  warnings: [{ code: 'WORK_MEM_HIGH', severity: 'warning', message: 'work_mem is high' }],
  blockers: [],
  estimates: { outputPath: 'C:\\exports\\orders.csv' },
  sessions: [],
  elapsedMs: BigInt(12),
});

describe('QueryTabView', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('starts with the export panel collapsed and the editor holding the doc SQL', () => {
    const { tabId } = setup({ sql: ORDERS });
    expect(doc(tabId)).toMatchObject({ panel: 'output', panelCollapsed: true });
    expect(panel()).toBeNull();
    expect(screen.getByRole('textbox', { name: qs('sql') })).toHaveTextContent(ORDERS);
    expect(button(qs('run'))).toBeEnabled();
  });

  it('Run calls export_preflight, then export_start, and opens 작업', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    await user.click(button(qs('run')));
    await waitFor(() => expect(mockBackend.calls).toContain('export_start'));
    expect(mockBackend.calls.indexOf('export_start')).toBeGreaterThan(mockBackend.calls.indexOf('export_preflight'));
    expect(doc(tabId).lastJobId).toMatch(/^job-/);
    expect(useExtractStore.getState().preflightDialog).toBeNull();
    expect(panel()).toHaveAttribute('data-panel', 'job');
  });

  it('opens the preflight sheet on warnings and starts only after acknowledgement', async () => {
    const user = userEvent.setup();
    setup({ sql: ORDERS, session: { workMem: '512MB' } });
    await user.click(button(qs('run')));
    const sheet = await screen.findByRole('dialog');
    expect(within(sheet).getByText(qs('preflightTitle'))).toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('export_start');
    const start = within(sheet).getByRole('button', { name: qs('start') });
    expect(start).toBeDisabled();
    await user.click(within(sheet).getByRole('checkbox', { name: qs('acknowledgeAria', { code: 'WORK_MEM_HIGH' }) }));
    expect(start).toBeEnabled();
    await user.click(start);
    await waitFor(() => expect(mockBackend.calls).toContain('export_start'));
    expect(useExtractStore.getState().preflightDialog).toBeNull();
  });

  it('Explain shows the plan tree in the 계획 panel', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    await user.click(button(qs('explain')));
    await waitFor(() => expect(mockBackend.calls).toContain('query_explain'));
    expect(await screen.findByText('Seq Scan')).toBeInTheDocument();
    expect(doc(tabId)).toMatchObject({ panel: 'plan', panelCollapsed: false });
    expect(segment(qs('segPlan'))).toHaveAttribute('aria-selected', 'true');
  });

  it('Check shows the result in 검사 with a badge on the segment', async () => {
    const user = userEvent.setup();
    setup({ sql: ORDERS });
    await user.click(button(qs('check')));
    expect(await screen.findByText(qs('checkOk', { kind: 'SELECT' }))).toBeInTheDocument();
    expect(within(segment(qs('segCheck'))).getByRole('img', { name: qs('badgeCheckOk') })).toBeInTheDocument();
  });

  it('lists check issues with their codes', async () => {
    const user = userEvent.setup();
    setup({ sql: 'DELETE FROM orders' });
    await user.click(button(qs('check')));
    expect(await screen.findByText(qs('checkIssues'), { selector: 'span' })).toBeInTheDocument();
    expect(within(segment(qs('segCheck'))).getByRole('img', { name: qs('badgeCheckFailed') })).toBeInTheDocument();
  });

  it('shows a dismissable error instead of calling the backend when the SQL is empty', async () => {
    const user = userEvent.setup();
    const { tabId } = setup();
    await user.click(button(qs('run')));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(doc(tabId).error ?? '-');
    expect(mockBackend.calls).not.toContain('export_preflight');
    await user.click(within(alert).getByRole('button'));
    expect(doc(tabId).error).toBeNull();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('toggles the panel by clicking the active segment and opens it on another one', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    await user.click(segment(qs('segOutput')));
    expect(panel()).toHaveAttribute('data-panel', 'output');
    expect(screen.getByRole('group', { name: qs('output') })).toBeInTheDocument();
    await user.click(segment(qs('segOutput')));
    expect(panel()).toBeNull();
    await user.click(segment(qs('segSession')));
    expect(panel()).toHaveAttribute('data-panel', 'session');
    expect(doc(tabId)).toMatchObject({ panel: 'session', panelCollapsed: false });
    // Ctrl+` goes through the same store toggle.
    act(() => useExtractStore.getState().togglePanel(tabId));
    expect(panel()).toBeNull();
  });

  it('routes Mod-Enter, Mod-E and Mod-Shift-Enter in the editor to this tab', async () => {
    const { tabId } = setup({ sql: ORDERS });
    const run = vi.spyOn(useExtractStore.getState(), 'run');
    const editor = screen.getByRole('textbox', { name: qs('sql') });
    fireEvent.keyDown(editor, { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true, shiftKey: true });
    await waitFor(() => expect(doc(tabId).lastCheck?.ok).toBe(true));
    fireEvent.keyDown(editor, { key: 'e', code: 'KeyE', keyCode: 69, ctrlKey: true });
    await waitFor(() => expect(doc(tabId).lastPlan).not.toBeNull());
    fireEvent.keyDown(editor, { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true });
    await waitFor(() => expect(mockBackend.calls).toContain('export_start'));
    expect(run).toHaveBeenCalledWith(tabId);
  });

  it('keeps each tab on its own document', async () => {
    const user = userEvent.setup();
    const a = openTab({ sql: ORDERS, name: 'a' });
    const b = openTab({ sql: 'SELECT 1', name: 'b' });
    render(
      <TooltipProvider>
        <QueryTabView tabId={a} />
        <QueryTabView tabId={b} />
      </TooltipProvider>,
    );
    const [, checkB] = screen.getAllByRole('button', { name: qs('check') });
    await user.click(checkB);
    await waitFor(() => expect(doc(b).lastCheck).not.toBeNull());
    expect(doc(a).lastCheck).toBeNull();
    expect(doc(a).panelCollapsed).toBe(true);
  });

  it('turns 실행 into 중지 while the job runs, with the inline confirm following job:* events', async () => {
    const user = userEvent.setup();
    const cancel = vi.fn(async () => undefined);
    useJobsStore.setState({ cancel });
    const { tabId } = setup({ sql: ORDERS });
    act(() => useJobsStore.getState().upsertSummary(job('job-1', 'running')));
    attachJob(tabId, 'job-1');

    expect(screen.queryByRole('button', { name: qs('run') })).toBeNull();
    expect(within(segment(qs('segJob'))).getByRole('img', { name: qs('badgeJobActive') })).toBeInTheDocument();
    await user.click(button(qs('stop')));
    expect(useUiStore.getState().cancelConfirmJobId).toBe('job-1');
    expect(screen.getByText(qs('cancelAsk'))).toBeInTheDocument();
    await user.click(button(qs('no')));
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();

    await user.click(button(qs('stop')));
    await user.click(button(qs('yes')));
    expect(cancel).toHaveBeenCalledWith('job-1');
    // Rust decides: until job:state arrives the job is still running and 중지 stays.
    expect(button(qs('stop'))).toBeInTheDocument();
    expect(useJobsStore.getState().jobs['job-1'].summary.state).toBe('running');

    act(() => useJobsStore.getState().applyState({ jobId: 'job-1', state: 'cancelled', at: '2026-10-27T10:01:00Z' }));
    expect(button(qs('run'))).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: qs('stop') })).toBeNull();
  });

  it('drops the inline confirm once the job ends on its own', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    act(() => useJobsStore.getState().upsertSummary(job('job-1', 'running')));
    attachJob(tabId, 'job-1');
    await user.click(button(qs('stop')));
    expect(screen.getByText(qs('cancelAsk'))).toBeInTheDocument();
    act(() => useJobsStore.getState().applyState({ jobId: 'job-1', state: 'done', at: '2026-10-27T10:01:00Z' }));
    expect(screen.queryByText(qs('cancelAsk'))).toBeNull();
    expect(button(qs('run'))).toBeInTheDocument();
  });

  it('does not start a second export from the editor while the job runs', () => {
    const { tabId } = setup({ sql: ORDERS });
    act(() => useJobsStore.getState().upsertSummary(job('job-1', 'running')));
    attachJob(tabId, 'job-1');
    const run = vi.spyOn(useExtractStore.getState(), 'run');
    fireEvent.keyDown(screen.getByRole('textbox', { name: qs('sql') }), { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true });
    expect(run).not.toHaveBeenCalled();
  });

  it('opens 작업 with the warned strip when the job starts waiting; 검토 opens the resume sheet', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    act(() => useJobsStore.getState().upsertSummary(job('job-2', 'preflight')));
    attachJob(tabId, 'job-2');
    expect(panel()).toBeNull();

    act(() => useJobsStore.getState().upsertSummary(job('job-2', 'warned', { preflight: warnedReport() })));
    expect(panel()).toHaveAttribute('data-panel', 'job');
    expect(screen.getByText(qs('warnedStrip', { n: 1 }))).toBeInTheDocument();
    expect(within(segment(qs('segJob'))).getByRole('img', { name: qs('badgeJobWarned') })).toBeInTheDocument();
    // The strip only opens the sheet; the job state is untouched.
    await user.click(button(qs('review')));
    const sheet = await screen.findByRole('dialog');
    expect(within(sheet).getByRole('button', { name: qs('resume') })).toBeDisabled();
    expect(useJobsStore.getState().jobs['job-2'].summary.state).toBe('warned');
  });

  it('shows progress, rates and files of the job as the events report them', () => {
    const { tabId } = setup({ sql: ORDERS });
    act(() => {
      useJobsStore.getState().upsertSummary(job('job-3', 'running'));
      useJobsStore.getState().applyProgress({
        jobId: 'job-3',
        rows: BigInt(19_800_000),
        bytesIn: BigInt(0),
        bytesOut: BigInt(1_100_000_000),
        batches: BigInt(10),
        files: 1,
        elapsedMs: BigInt(291_000),
        rateRowsPerSec: 412_000,
        rateBytesPerSec: 98_000_000,
        etaSec: BigInt(192),
        phase: 'transferring',
      });
    });
    attachJob(tabId, 'job-3');
    act(() => useExtractStore.getState().setPanel(tabId, 'job'));
    const line = screen.getByTestId('job-progress');
    expect(line).toHaveTextContent('19.8M');
    expect(line).toHaveTextContent('4:51');
    expect(line).toHaveTextContent('3:12');
    expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow');

    act(() =>
      useJobsStore.getState().applyDone({
        jobId: 'job-3',
        state: 'done',
        result: { rows: BigInt(30_000_000), bytes: BigInt(1_600_000_000), files: ['C:\\exports\\orders_part0001.csv'], durationMs: BigInt(400_000), notes: ['verified'] },
      }),
    );
    expect(screen.getByText('orders_part0001.csv')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: qs('revealFile') })).toBeInTheDocument();
  });

  it('applies the Excel preset from the 출력 panel', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    act(() => useExtractStore.getState().setPanel(tabId, 'output'));
    await user.click(button(qs('excelPreset')));
    const out = doc(tabId).output;
    expect(out).toMatchObject({ format: 'csv', csv: { bom: true, crlf: true, header: true } });
    expect(out.split?.rows).toBe(BigInt(1_000_000));
    expect(outputSummary(out).text).toContain('BOM');
  });

  it('edits the name and the folder of this doc from 출력', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    act(() => useExtractStore.getState().setPanel(tabId, 'output'));
    await user.type(screen.getByLabelText(qs('nameLabel')), 'orders');
    expect(doc(tabId).name).toBe('orders');
  });

  it('disables the session options the driver does not support', () => {
    const { tabId } = setup({ sql: 'SELECT 1' }, 'c-local');
    act(() => useExtractStore.getState().setPanel(tabId, 'session'));
    expect(screen.getByLabelText(qs('workMem'))).toBeDisabled();
    expect(screen.getByLabelText(qs('fetchBatch'))).toBeEnabled();
  });

  it('switches to file mode and loads the file into a read-only editor', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    act(() => useExtractStore.getState().setSqlMode(tabId, 'file'));
    expect(screen.getByRole('button', { name: `${qs('modeMenu')}: ${qs('modeFile')}` })).toBeInTheDocument();
    await user.type(screen.getByRole('textbox', { name: qs('filePath') }), 'C:\\q\\orders.sql');
    await user.click(button(qs('loadFile')));
    await waitFor(() => expect(doc(tabId).sqlFileInfo).not.toBeNull());
    const content = await screen.findByRole('textbox', { name: qs('fileContent') });
    expect(content).toHaveAttribute('aria-readonly', 'true');
    // The typed SQL survives the round trip.
    act(() => useExtractStore.getState().setSqlMode(tabId, 'text'));
    expect(screen.getByRole('textbox', { name: qs('sql') })).toHaveTextContent(ORDERS);
  });

  it('opens the mode menu with the file and other-connection entries', async () => {
    const user = userEvent.setup();
    const { tabId } = setup({ sql: ORDERS });
    await user.click(screen.getByRole('button', { name: `${qs('modeMenu')}: ${qs('modeSql')}` }));
    await user.click(await screen.findByRole('menuitem', { name: new RegExp(qs('openInConnection')) }));
    expect(useUiStore.getState().palette).toMatchObject({ open: true, scope: 'connections', pick: { kind: 'openInConnection', tabId } });
  });

  it('clamps the panel height to the limits and half of the tab', () => {
    expect(clampPanelHeight(50, 1000)).toBe(120);
    expect(clampPanelHeight(900, 1000)).toBe(500);
    expect(clampPanelHeight(900, 2000)).toBe(640);
    expect(clampPanelHeight(300, 0)).toBe(300);
  });

  it('resizes the panel from the keyboard on the splitter', () => {
    const { tabId } = setup({ sql: ORDERS });
    act(() => useExtractStore.getState().setPanel(tabId, 'output'));
    const before = useUiStore.getState().exportPanelHeight;
    fireEvent.keyDown(screen.getByRole('separator', { name: qs('resize') }), { key: 'ArrowUp' });
    expect(useUiStore.getState().exportPanelHeight).toBe(before + 16);
  });
});
