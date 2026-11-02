import type { Connection } from 'ipc-types';
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { ConnectionDialog } from './ConnectionDialog';
import { cs } from './ConnectionDialog.strings';

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

function renderDialog(editId: string | null) {
  act(() => useUiStore.getState().openConnectionDialog(editId));
  return render(
    <TooltipProvider>
      <ConnectionDialog />
    </TooltipProvider>,
  );
}

const button = (name: string) => screen.getByRole('button', { name });
const dialogOpen = () => useUiStore.getState().connectionDialog.open;
const conn = (id: string) => useConnectionsStore.getState().connections.find((c) => c.id === id);

function setup() {
  resetStores();
  mockBackend.reset();
  mockBackend.configure({ latencyMs: 0 });
  useConnectionsStore.setState({ connections: [...mockConnections(), oracleConn], drivers: mockDrivers(), loaded: true });
}

describe('ConnectionDialog: new connection (driver first)', () => {
  beforeEach(setup);

  it('starts with the driver picker; unavailable drivers are dimmed but selectable; search + Enter go to the form', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    const grid = screen.getByRole('listbox', { name: cs('driverList') });
    const tiles = within(grid).getAllByRole('option');
    expect(tiles.map((t) => t.getAttribute('data-driver'))).toEqual(['postgres', 'mysql', 'mssql', 'sqlite', 'oracle', 'odbc']);
    expect(tiles[0]).toHaveAttribute('aria-selected', 'true');
    expect(tiles[4]).toHaveTextContent(cs('needsInstall'));
    expect(tiles[0]).not.toHaveTextContent(cs('needsInstall'));

    // The search field has focus; typing filters and selects the first hit.
    await user.keyboard('sqlite');
    expect(within(grid).getAllByRole('option')).toHaveLength(1);
    await user.keyboard('{Enter}');
    expect(screen.getByLabelText(cs('name'))).toHaveFocus();
    expect(screen.getByLabelText(cs('field.path'))).toBeInTheDocument();
    expect(screen.queryByLabelText(cs('host'))).toBeNull();
  });

  it('arrow keys move by tile and by row; an unavailable driver opens the form with the install notice', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    const grid = screen.getByRole('listbox', { name: cs('driverList') });
    act(() => grid.focus());
    await user.keyboard('{ArrowRight}');
    expect(grid.querySelector('[aria-selected="true"]')).toHaveAttribute('data-driver', 'mysql');
    await user.keyboard('{ArrowDown}');
    expect(grid.querySelector('[aria-selected="true"]')).toHaveAttribute('data-driver', 'oracle');
    await user.keyboard('{Enter}');
    expect(screen.getByText(cs('driverUnavailable'))).toBeInTheDocument();
    // Saving stays blocked while the driver is missing.
    await user.type(screen.getByLabelText(cs('name')), 'erp2');
    await user.click(button(cs('save')));
    expect(await screen.findAllByText(cs('driverUnavailable'))).toHaveLength(2);
    expect(mockBackend.calls).not.toContain('conn_save');
  });

  it('↓ in the driver search enters the grid; Esc closes the sheet without saving', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    const grid = screen.getByRole('listbox', { name: cs('driverList') });
    expect(screen.getByRole('searchbox', { name: cs('driverSearch') })).toHaveFocus();
    await user.keyboard('{ArrowDown}');
    expect(grid).toHaveFocus();
    await user.keyboard('{End}');
    expect(grid.querySelector('[aria-selected="true"]')).toHaveAttribute('data-driver', 'odbc');
    await user.keyboard('{Escape}');
    expect(dialogOpen()).toBe(false);
    expect(mockBackend.calls).not.toContain('conn_save');
  });

  it('변경 returns to the picker and keeps what was typed; the port follows the new default', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    await user.click(button(cs('next')));
    expect(screen.getByLabelText(cs('port'))).toHaveValue('5432');
    await user.type(screen.getByLabelText(cs('name')), 'shop');
    await user.type(screen.getByLabelText(cs('host')), 'mysql.internal');
    await user.click(button(cs('change')));
    await user.dblClick(within(screen.getByRole('listbox', { name: cs('driverList') })).getByText('MySQL / MariaDB'));
    expect(screen.getByLabelText(cs('name'))).toHaveValue('shop');
    expect(screen.getByLabelText(cs('host'))).toHaveValue('mysql.internal');
    expect(screen.getByLabelText(cs('port'))).toHaveValue('3306');
  });

  it('rescans drivers from the picker footer', async () => {
    const user = userEvent.setup();
    mockBackend.installDriver('oracle');
    renderDialog(null);
    await user.click(button(cs('rescan')));
    await waitFor(() => expect(mockBackend.calls).toContain('drivers_rescan'));
    await waitFor(() => expect(screen.getByRole('listbox', { name: cs('driverList') })).not.toHaveTextContent(cs('needsInstall')));
    expect(useToastStore.getState().toasts.at(-1)?.message).toMatch(/6개 사용 가능/);
  });

  it('a new connection is saved with its tag and Connect opens its workspace', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    await user.click(button(cs('next')));
    await user.type(screen.getByLabelText(cs('name')), 'reporting');
    await user.type(screen.getByLabelText(cs('host')), 'db.internal');
    await user.click(screen.getByRole('combobox', { name: '환경 태그' }));
    await user.click(await screen.findByRole('option', { name: /staging/ }));
    await user.click(button(cs('connect')));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    const saved = useConnectionsStore.getState().connections.find((c) => c.name === 'reporting');
    expect(saved).toMatchObject({ tag: 'staging', host: 'db.internal', port: 5432, driver: 'postgres' });
    expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: saved!.id });
  });

  it('Enter in a text field runs the primary action (Connect)', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    await user.click(button(cs('next')));
    await user.type(screen.getByLabelText(cs('name')), 'quick{Enter}');
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(useWorkspaceStore.getState().openConnId).toBe(useConnectionsStore.getState().connections.find((c) => c.name === 'quick')?.id);
  });

  it('requires a name', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    await user.click(button(cs('next')));
    await user.click(button(cs('save')));
    expect(await screen.findByText(cs('nameRequired'))).toBeInTheDocument();
    expect(dialogOpen()).toBe(true);
  });
});

describe('ConnectionDialog: edit, test and connect', () => {
  beforeEach(setup);

  it('edits open the form directly; the tag can be changed and cleared by omission', async () => {
    const user = userEvent.setup();
    renderDialog('c-local');
    expect(screen.queryByRole('listbox', { name: cs('driverList') })).toBeNull();
    expect(screen.getByLabelText(cs('name'))).toHaveValue('local (SQLite)');
    await user.click(screen.getByRole('combobox', { name: '환경 태그' }));
    await user.click(await screen.findByRole('option', { name: /production/ }));
    await user.click(button(cs('save')));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(conn('c-local')?.tag).toBe('production');

    renderDialog('c-local');
    await user.click(screen.getAllByRole('combobox', { name: '환경 태그' }).at(-1)!);
    await user.click(await screen.findByRole('option', { name: '태그 없음' }));
    await user.click(screen.getAllByRole('button', { name: cs('save') }).at(-1)!);
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(conn('c-local')).not.toHaveProperty('tag');
  });

  it('Test saves first, shows the inline result and clears the typed secret', async () => {
    const user = userEvent.setup();
    renderDialog('c-staging');
    await user.type(screen.getByLabelText(cs('password')), 'new-secret');
    await user.click(button(cs('test')));
    expect(await screen.findByText(/PostgreSQL .+ · \d/)).toBeInTheDocument();
    const calls = mockBackend.calls;
    expect(calls.indexOf('conn_save')).toBeGreaterThanOrEqual(0);
    expect(calls.indexOf('conn_save')).toBeLessThan(calls.indexOf('conn_test'));
    expect(screen.getByLabelText(cs('password'))).toHaveValue('');
    expect(screen.getByText(/work_mem 권장/)).toBeInTheDocument();
    expect(dialogOpen()).toBe(true);
  });

  it('a failed test shows the error inline in danger text', async () => {
    const user = userEvent.setup();
    renderDialog('c-staging');
    await user.click(screen.getByLabelText(cs('savePassword')));
    await user.type(screen.getByLabelText(cs('password')), 'wrong');
    await user.click(button(cs('test')));
    const alert = await screen.findByText(/실패: .*password authentication failed/);
    expect(alert.closest('[role="alert"]')).toHaveClass('text-danger');
  });

  it('Connect with an ask-every-time password tests with it first (unlocking the session), then opens the workspace', async () => {
    const user = userEvent.setup();
    renderDialog('c-staging');
    await user.click(screen.getByLabelText(cs('savePassword')));
    expect(screen.getByText(cs('savePasswordOff'))).toBeInTheDocument();
    await user.type(screen.getByLabelText(cs('password')), 'once');
    await user.click(button(cs('connect')));
    await waitFor(() => expect(useWorkspaceStore.getState().openConnId).toBe('c-staging'));
    expect(mockBackend.calls).toContain('conn_test');
    expect(useConnectionsStore.getState().unlocked['c-staging']).toBe(true);
    expect(conn('c-staging')?.savePassword).toBe(false);
    expect(mockBackend.storedSecrets('c-staging').password).toBe(false);
    expect(dialogOpen()).toBe(false);
  });

  it('editing a connection whose workspace is open: 저장 is primary, 연결 hidden, and metadata reloads', async () => {
    const user = userEvent.setup();
    await useWorkspaceStore.getState().openWorkspace('c-staging');
    renderDialog('c-staging');
    expect(screen.queryByRole('button', { name: cs('connect') })).toBeNull();
    expect(button(cs('save')).className).toContain('bg-accent-fill');
    mockBackend.calls.length = 0;
    await user.clear(screen.getByLabelText(cs('host')));
    await user.type(screen.getByLabelText(cs('host')), 'db-staging-2.internal');
    await user.click(button(cs('save')));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_invalidate'));
    expect(useToastStore.getState().toasts.some((t) => t.message === '연결 설정이 바뀌어 메타데이터를 다시 불러옵니다')).toBe(true);
  });

  it('the SSH section is collapsible behind its switch', async () => {
    const user = userEvent.setup();
    renderDialog('c-staging');
    expect(screen.queryByLabelText(cs('sshHost'))).toBeNull();
    await user.click(screen.getByRole('switch', { name: cs('sshToggle') }));
    expect(screen.getByLabelText(cs('sshHost'))).toBeInTheDocument();
    await user.click(screen.getByRole('radio', { name: cs('authPassword') }));
    expect(screen.getByLabelText(cs('sshPassword'))).toBeInTheDocument();
    expect(screen.queryByLabelText(cs('sshPassphrase'))).toBeNull();
    await user.click(screen.getByRole('button', { name: cs('ssh'), expanded: true }));
    expect(screen.queryByLabelText(cs('sshHost'))).toBeNull();
  });

  it('rescans drivers from the driver-unavailable notice so the connection can be saved without a restart', async () => {
    const user = userEvent.setup();
    renderDialog('c-erp');
    expect(screen.getByText(cs('driverUnavailable'))).toBeInTheDocument();

    // Rescanning before installing anything changes nothing.
    await user.click(button(cs('rescan')));
    await waitFor(() => expect(mockBackend.calls).toContain('drivers_rescan'));
    await waitFor(() => expect(button(cs('rescan'))).toBeEnabled());
    expect(screen.getByText(cs('driverUnavailable'))).toBeInTheDocument();

    // After the user installs Instant Client, a rescan picks it up and the notice goes away.
    mockBackend.installDriver('oracle');
    await user.click(button(cs('rescan')));
    await waitFor(() => expect(screen.queryByText(cs('driverUnavailable'))).not.toBeInTheDocument());
    expect(useConnectionsStore.getState().drivers.find((d) => d.id === 'oracle')?.available).toBe(true);
  });
});

describe('ConnectionDialog: stored secrets and links', () => {
  beforeEach(setup);
  afterEach(() => vi.restoreAllMocks());

  it('deletes only the stored database password when its checkbox is ticked', async () => {
    const user = userEvent.setup();
    renderDialog('c-prod');
    // c-prod stores a password but no SSH secret (key auth without passphrase).
    const clear = await screen.findByRole('checkbox', { name: cs('clearPassword') });
    expect(screen.queryByRole('checkbox', { name: cs('clearSshSecret') })).not.toBeInTheDocument();
    expect(screen.getByLabelText(cs('password'))).toHaveAttribute('placeholder', cs('passwordKeep'));
    await waitFor(() => expect(mockBackend.calls).toContain('conn_get'));

    await user.type(screen.getByLabelText(cs('password')), 'typed');
    await user.click(clear);
    // The typed password is dropped and the field locked: the backend refuses both at once.
    expect(screen.getByLabelText(cs('password'))).toBeDisabled();
    expect(screen.getByLabelText(cs('password'))).toHaveValue('');

    await user.click(button(cs('save')));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(mockBackend.storedSecrets('c-prod').password).toBe(false);
    expect(conn('c-prod')?.hasPassword).toBe(false);
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

    await user.click(await screen.findByRole('checkbox', { name: cs('clearSshSecret') }));
    expect(screen.getByLabelText(cs('sshPassword'))).toBeDisabled();
    await user.click(button(cs('save')));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(mockBackend.storedSecrets(saved.id)).toMatchObject({ password: true, sshSecret: false });
    const after = conn(saved.id);
    expect(after?.hasPassword).toBe(true);
    expect(after?.hasSshSecret).toBe(false);
  });

  it('offers no delete option for a connection without stored secrets', async () => {
    renderDialog('c-local');
    await waitFor(() => expect(mockBackend.calls).toContain('conn_get'));
    expect(screen.queryByRole('checkbox', { name: cs('clearPassword') })).not.toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: cs('clearSshSecret') })).not.toBeInTheDocument();
  });

  it('shows an inline notice and logs it when the install link cannot be opened', async () => {
    const user = userEvent.setup();
    vi.spyOn(window, 'open').mockImplementation(() => {
      throw new Error('pop-up blocked');
    });
    renderDialog('c-erp');
    await user.click(screen.getByRole('button', { name: /Oracle Instant Client 21/ }));
    expect(await screen.findByText(/Could not open https:\/\/www\.oracle\.com\/.*pop-up blocked/)).toBeInTheDocument();
    expect(useLogsStore.getState().entries.some((e) => e.level === 'error' && e.message.includes('pop-up blocked'))).toBe(true);
  });
});
