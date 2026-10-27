import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { t } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { defaultOutputOpts, useExtractStore } from '@/stores/extract';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ExtractTab } from './ExtractTab';
import { PreflightDialog } from './PreflightDialog';

function setup(sql: string, workMem = '64MB') {
  useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  useExtractStore.setState({
    connId: 'c-prod',
    sqlMode: 'text',
    sql,
    session: { workMem },
    output: { ...defaultOutputOpts(), dir: 'C:\\exports' },
  });
  return render(
    <TooltipProvider>
      <ExtractTab />
      <PreflightDialog />
    </TooltipProvider>,
  );
}

describe('ExtractTab', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
  });

  it('Run calls export_preflight and then export_start', async () => {
    const user = userEvent.setup();
    setup('SELECT * FROM public.orders');
    await user.click(screen.getByRole('button', { name: new RegExp(`^${t('extract.run')}`) }));
    await waitFor(() => expect(mockBackend.calls).toContain('export_start'));
    const preflightAt = mockBackend.calls.indexOf('export_preflight');
    const startAt = mockBackend.calls.indexOf('export_start');
    expect(preflightAt).toBeGreaterThanOrEqual(0);
    expect(startAt).toBeGreaterThan(preflightAt);
    expect(useExtractStore.getState().lastJobId).toMatch(/^job-/);
    expect(useExtractStore.getState().preflightDialog).toBeNull();
  });

  it('opens the preflight dialog on warnings and starts only after acknowledgement', async () => {
    const user = userEvent.setup();
    setup('SELECT * FROM public.orders', '512MB');
    await user.click(screen.getByRole('button', { name: new RegExp(`^${t('extract.run')}`) }));
    await waitFor(() => expect(useExtractStore.getState().preflightDialog).not.toBeNull());
    expect(mockBackend.calls).not.toContain('export_start');
    const start = await screen.findByRole('button', { name: t('extract.startAnyway') });
    expect(start).toBeDisabled();
    await user.click(screen.getByRole('checkbox', { name: /WORK_MEM_HIGH/ }));
    expect(start).toBeEnabled();
    await user.click(start);
    await waitFor(() => expect(mockBackend.calls).toContain('export_start'));
    expect(useExtractStore.getState().preflightDialog).toBeNull();
  });

  it('EXPLAIN calls query_explain and shows the plan summary', async () => {
    const user = userEvent.setup();
    setup('SELECT * FROM public.orders');
    await user.click(screen.getByRole('button', { name: new RegExp(`^${t('extract.explain')}`) }));
    await waitFor(() => expect(mockBackend.calls).toContain('query_explain'));
    expect(await screen.findByText('Seq Scan')).toBeInTheDocument();
  });

  it('shows an error instead of calling the backend when the SQL is empty', async () => {
    const user = userEvent.setup();
    setup('');
    await user.click(screen.getByRole('button', { name: new RegExp(`^${t('extract.run')}`) }));
    expect(await screen.findByText(t('extract.emptySql'))).toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('export_preflight');
  });
});
