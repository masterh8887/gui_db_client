import type { Connection } from 'ipc-types';
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { connSave } from '@/lib/commands';
import { t } from '@/lib/i18n';
import { mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { defaultOutputOpts, useExtractStore } from '@/stores/extract';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { ConnectionTree } from '@/components/tree/ConnectionTree';
import { TooltipProvider } from '@/components/ui';
import { DbPasswordDialog } from './DbPasswordDialog';

/** A PostgreSQL connection saved with "ask every time" (no password in the keyring). */
async function askEveryTimeConnection(): Promise<Connection> {
  const saved = await connSave({
    id: '',
    name: 'ask (PG)',
    driver: 'postgres',
    host: 'db-replica.internal',
    port: 5432,
    database: 'orders',
    user: 'analyst',
    fields: {},
    session: {},
    savePassword: false,
    hasPassword: false,
    hasSshSecret: false,
  });
  useConnectionsStore.setState({ connections: [saved], drivers: mockDrivers(), selectedId: saved.id, loaded: true });
  mockBackend.calls.length = 0;
  return saved;
}

function renderWithDialog(ui: React.ReactNode) {
  return render(
    <TooltipProvider>
      {ui}
      <DbPasswordDialog />
    </TooltipProvider>,
  );
}

describe('DbPasswordDialog ("ask every time" connections)', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
  });

  it('asks before browsing, sends the password once through conn_test, then expands', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    renderWithDialog(<ConnectionTree />);

    await user.click(screen.getByText('ask (PG)'));
    expect(await screen.findByRole('dialog', { name: t('dbpw.title') })).toBeInTheDocument();
    expect(useUiStore.getState().dbPasswordPrompt).toEqual({ connId: conn.id, reason: 'browse' });
    expect(mockBackend.calls).not.toContain('catalog_databases');

    await user.type(screen.getByLabelText(/analyst/), 's3cret');
    await user.click(screen.getByRole('button', { name: t('dbpw.submit') }));

    expect(await screen.findByText('orders')).toBeInTheDocument();
    expect(mockBackend.calls.indexOf('conn_test')).toBeLessThan(mockBackend.calls.indexOf('catalog_databases'));
    expect(useConnectionsStore.getState().unlocked[conn.id]).toBe(true);
    expect(useUiStore.getState().dbPasswordPrompt).toBeNull();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('keeps the dialog open with the error when the password is wrong', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    renderWithDialog(<ConnectionTree />);

    await user.click(screen.getByText('ask (PG)'));
    await user.type(await screen.findByLabelText(/analyst/), 'wrong');
    await user.click(screen.getByRole('button', { name: t('dbpw.submit') }));

    expect(await screen.findByText(/password authentication failed/)).toBeInTheDocument();
    expect(useUiStore.getState().dbPasswordPrompt?.connId).toBe(conn.id);
    expect(useConnectionsStore.getState().unlocked[conn.id]).toBeUndefined();
    // The typed secret is not kept in the form after the attempt.
    expect(screen.getByLabelText(/analyst/)).toHaveValue('');
  });

  it('holds Run until the password is accepted, then preflights', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    useExtractStore.setState({ connId: conn.id, sqlMode: 'text', sql: 'SELECT * FROM public.orders', output: { ...defaultOutputOpts(), dir: 'C:/exports' } });
    renderWithDialog(null);

    await act(() => useExtractStore.getState().run());
    expect(useUiStore.getState().dbPasswordPrompt).toEqual({ connId: conn.id, reason: 'run' });
    expect(mockBackend.calls).not.toContain('export_preflight');

    await user.type(await screen.findByLabelText(/analyst/), 's3cret');
    await user.click(screen.getByRole('button', { name: t('dbpw.submit') }));

    await waitFor(() => expect(mockBackend.calls).toContain('export_preflight'));
    expect(mockBackend.calls.indexOf('conn_test')).toBeLessThan(mockBackend.calls.indexOf('export_preflight'));
  });
});
