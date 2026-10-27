import type { Connection } from 'ipc-types';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { t } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ConnectionDialog } from './ConnectionDialog';

/** An imported Oracle connection whose vendor client is not installed yet. */
const oracleConn: Connection = {
  id: 'c-erp',
  name: 'erp (Oracle)',
  driver: 'oracle',
  host: 'erp-db.internal',
  port: 1521,
  user: 'erp_ro',
  fields: { service_name: 'ERP' },
  session: {},
  savePassword: true,
  hasPassword: false,
  hasSshSecret: false,
};

describe('ConnectionDialog', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: [...mockConnections(), oracleConn], drivers: mockDrivers(), loaded: true });
  });

  it('rescans drivers from the driver-unavailable notice so the connection can be saved without a restart', async () => {
    const user = userEvent.setup();
    useUiStore.getState().openConnectionDialog('c-erp');
    render(
      <TooltipProvider>
        <ConnectionDialog />
      </TooltipProvider>,
    );
    expect(screen.getByText(t('conn.driverUnavailable'))).toBeInTheDocument();

    // Rescanning before installing anything changes nothing.
    await user.click(screen.getByRole('button', { name: t('drivers.rescan') }));
    await waitFor(() => expect(mockBackend.calls).toContain('drivers_rescan'));
    await waitFor(() => expect(screen.getByRole('button', { name: t('drivers.rescan') })).toBeEnabled());
    expect(screen.getByText(t('conn.driverUnavailable'))).toBeInTheDocument();

    // After the user installs Instant Client, a rescan picks it up and the notice goes away.
    mockBackend.installDriver('oracle');
    await user.click(screen.getByRole('button', { name: t('drivers.rescan') }));
    await waitFor(() => expect(screen.queryByText(t('conn.driverUnavailable'))).not.toBeInTheDocument());
    expect(useConnectionsStore.getState().drivers.find((d) => d.id === 'oracle')?.available).toBe(true);
  });
});

const CLEAR_PASSWORD = /저장된 비밀번호 삭제|Delete the stored password/;
const CLEAR_SSH = /저장된 SSH 비밀 삭제|Delete the stored SSH secret/;

function renderDialog(editId: string) {
  useUiStore.getState().openConnectionDialog(editId);
  return render(
    <TooltipProvider>
      <ConnectionDialog />
    </TooltipProvider>,
  );
}

describe('ConnectionDialog: stored secrets and links', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });
  afterEach(() => vi.restoreAllMocks());

  it('deletes only the stored database password when its checkbox is ticked', async () => {
    const user = userEvent.setup();
    renderDialog('c-prod');
    // c-prod stores a password but no SSH secret (key auth without passphrase).
    const clear = await screen.findByRole('checkbox', { name: CLEAR_PASSWORD });
    expect(screen.queryByRole('checkbox', { name: CLEAR_SSH })).not.toBeInTheDocument();
    await waitFor(() => expect(mockBackend.calls).toContain('conn_get'));

    await user.type(screen.getByLabelText(t('conn.password')), 'typed');
    await user.click(clear);
    // The typed password is dropped and the field locked: the backend refuses both at once.
    expect(screen.getByLabelText(t('conn.password'))).toBeDisabled();
    expect(screen.getByLabelText(t('conn.password'))).toHaveValue('');

    await user.click(screen.getByRole('button', { name: t('conn.save') }));
    await waitFor(() => expect(useUiStore.getState().connectionDialog.open).toBe(false));
    expect(mockBackend.storedSecrets('c-prod').password).toBe(false);
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')?.hasPassword).toBe(false);
  });

  it('deletes only the stored SSH secret when its checkbox is ticked', async () => {
    const user = userEvent.setup();
    const saved = await connSave({
      id: '',
      name: 'tunnelled',
      driver: 'postgres',
      host: 'db',
      user: 'u',
      fields: {},
      ssh: { enabled: true, host: 'bastion', port: 22, user: 'deploy', auth: 'password', compression: true },
      session: {},
      savePassword: true,
      hasPassword: false,
      hasSshSecret: false,
      password: 'pw',
      sshPassword: 'sp',
    });
    useConnectionsStore.setState((s) => ({ connections: [...s.connections, saved] }));
    renderDialog(saved.id);

    await user.click(await screen.findByRole('checkbox', { name: CLEAR_SSH }));
    expect(screen.getByLabelText(t('conn.sshPassword'))).toBeDisabled();
    await user.click(screen.getByRole('button', { name: t('conn.save') }));
    await waitFor(() => expect(useUiStore.getState().connectionDialog.open).toBe(false));
    expect(mockBackend.storedSecrets(saved.id)).toMatchObject({ password: true, sshSecret: false });
    const after = useConnectionsStore.getState().connections.find((c) => c.id === saved.id);
    expect(after?.hasPassword).toBe(true);
    expect(after?.hasSshSecret).toBe(false);
  });

  it('offers no delete option for a connection without stored secrets', async () => {
    renderDialog('c-local');
    await waitFor(() => expect(mockBackend.calls).toContain('conn_get'));
    expect(screen.queryByRole('checkbox', { name: CLEAR_PASSWORD })).not.toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: CLEAR_SSH })).not.toBeInTheDocument();
  });

  it('shows an inline notice and logs it when the install link cannot be opened', async () => {
    const user = userEvent.setup();
    vi.spyOn(window, 'open').mockImplementation(() => {
      throw new Error('pop-up blocked');
    });
    useConnectionsStore.setState((s) => ({ connections: [...s.connections, oracleConn] }));
    renderDialog('c-erp');
    await user.click(screen.getByRole('button', { name: /Oracle Instant Client 21/ }));
    expect(await screen.findByText(/Could not open https:\/\/www\.oracle\.com\/.*pop-up blocked/)).toBeInTheDocument();
    expect(useLogsStore.getState().entries.some((e) => e.level === 'error' && e.message.includes('pop-up blocked'))).toBe(true);
  });
});
