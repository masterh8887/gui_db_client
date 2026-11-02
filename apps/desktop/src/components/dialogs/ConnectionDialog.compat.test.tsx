// Connection sheet: registry choices as selects and the 호환성 (compatibility) section of the
// MySQL legacy fields (docs/mysql-compat.md §3, §9).

import type { Connection } from 'ipc-types';
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { ConnectionDialog } from './ConnectionDialog';
import { cs } from './ConnectionDialog.strings';

/** A MySQL connection saved before the compatibility fields existed. */
const oldMysql: Connection = {
  id: 'c-shop',
  name: 'shop (MySQL)',
  driver: 'mysql',
  host: 'shop-db.internal',
  port: 3306,
  user: 'ro',
  fields: { sslmode: 'prefer' },
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

const compatToggle = () => screen.getByRole('button', { name: cs('compat') });
const dialogOpen = () => useUiStore.getState().connectionDialog.open;
const conn = (id: string) => useConnectionsStore.getState().connections.find((c) => c.id === id);

async function choose(user: ReturnType<typeof userEvent.setup>, field: string, option: string) {
  await user.click(screen.getByRole('combobox', { name: cs(`field.${field}` as Parameters<typeof cs>[0]) }));
  await user.click(await screen.findByRole('option', { name: option }));
}

describe('ConnectionDialog: MySQL compatibility fields', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection(), oldMysql], drivers: mockDrivers(), loaded: true });
  });

  it('a new MySQL connection keeps the section collapsed with the defaults, renders choices as selects and saves them', async () => {
    const user = userEvent.setup();
    renderDialog(null);
    await user.dblClick(within(screen.getByRole('listbox', { name: cs('driverList') })).getByText('MySQL / MariaDB'));

    // Collapsed by default: a one-line summary of the defaults, no controls.
    expect(compatToggle()).toHaveAttribute('aria-expanded', 'false');
    const section = screen.getByRole('region', { name: cs('compat') });
    expect(section).toHaveTextContent(`${cs('choice.protocol.auto')} · ${cs('choice.encoding.auto')}`);
    expect(screen.queryByRole('combobox', { name: cs('field.protocol') })).toBeNull();
    // SSL mode comes from the registry choices of mysql (no allow / verify-ca).
    await user.click(screen.getByRole('combobox', { name: cs('field.sslmode') }));
    expect((await screen.findAllByRole('option')).map((o) => o.textContent)).toEqual(['disable', 'prefer', 'require', 'verify-full']);
    await user.keyboard('{Escape}');

    await user.click(compatToggle());
    expect(screen.getByRole('combobox', { name: cs('field.protocol') })).toHaveTextContent(cs('choice.protocol.auto'));
    expect(screen.getByRole('combobox', { name: cs('field.encoding') })).toHaveTextContent(cs('choice.encoding.auto'));
    await user.click(screen.getByRole('combobox', { name: cs('field.protocol') }));
    expect((await screen.findAllByRole('option')).map((o) => o.textContent)).toEqual([
      cs('choice.protocol.auto'),
      cs('choice.protocol.modern'),
      cs('choice.protocol.legacy'),
    ]);
    await user.click(screen.getByRole('option', { name: cs('choice.protocol.legacy') }));
    await choose(user, 'encoding', cs('choice.encoding.euc-kr'));

    // Old passwords: a switch with the warning text, highlighted once on.
    const oldPw = screen.getByRole('switch', { name: cs('field.allow_old_password') });
    expect(oldPw).not.toBeChecked();
    expect(screen.getByText(cs('hint.allow_old_password'))).toBeInTheDocument();
    await user.click(oldPw);
    expect(oldPw).toBeChecked();
    expect(screen.getByText(cs('hint.allow_old_password')).parentElement).toHaveClass('text-warning');

    await user.type(screen.getByLabelText(cs('name')), 'legacy2');
    await user.click(screen.getByRole('button', { name: cs('save') }));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    const saved = useConnectionsStore.getState().connections.find((c) => c.name === 'legacy2');
    expect(saved?.fields).toEqual({ sslmode: 'prefer', protocol: 'legacy', encoding: 'euc-kr', allow_old_password: 'true' });
  });

  it('an existing connection without the fields shows and keeps the defaults', async () => {
    const user = userEvent.setup();
    renderDialog('c-shop');
    expect(compatToggle()).toHaveAttribute('aria-expanded', 'false');
    await user.click(compatToggle());
    expect(screen.getByRole('combobox', { name: cs('field.protocol') })).toHaveTextContent(cs('choice.protocol.auto'));
    expect(screen.getByRole('switch', { name: cs('field.allow_old_password') })).not.toBeChecked();
    await user.click(screen.getByRole('button', { name: cs('save') }));
    await waitFor(() => expect(dialogOpen()).toBe(false));
    expect(conn('c-shop')?.fields).toEqual({ sslmode: 'prefer', protocol: 'auto', encoding: 'auto', allow_old_password: 'false' });
  });

  it('a connection with non-default values opens the section', () => {
    renderDialog('c-legacy');
    expect(compatToggle()).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByRole('combobox', { name: cs('field.protocol') })).toHaveTextContent(cs('choice.protocol.legacy'));
    expect(screen.getByRole('combobox', { name: cs('field.encoding') })).toHaveTextContent(cs('choice.encoding.euc-kr'));
  });

  it('shows the core INVALID for a bad value next to its field; a valid choice clears it', async () => {
    useConnectionsStore.setState({
      connections: [...useConnectionsStore.getState().connections.filter((c) => c.id !== 'c-shop'), { ...oldMysql, fields: { protocol: 'v4' } }],
    });
    // The mock backend gets the same connection (Test saves first, then probes).
    const user = userEvent.setup();
    renderDialog('c-shop');
    // A value outside the choices is kept as-is (never silently rewritten) and opens the section.
    const protocol = screen.getByRole('combobox', { name: cs('field.protocol') });
    expect(protocol).toHaveTextContent('v4');
    await user.type(screen.getByLabelText(cs('password')), 'pw');
    await user.click(screen.getByRole('button', { name: cs('test') }));
    const inline = await screen.findByText("mysql field protocol: unknown value 'v4' (use auto, modern or legacy)");
    expect(inline.closest('[role="alert"]')).toHaveAttribute('id', 'conn-f-protocol-error');
    await choose(user, 'protocol', cs('choice.protocol.auto'));
    expect(screen.queryByText(/unknown value 'v4'/, { selector: '#conn-f-protocol-error span' })).toBeNull();
  });

  it('the test result names the pre-4.1 protocol of a legacy server', async () => {
    const user = userEvent.setup();
    renderDialog('c-legacy');
    await user.click(screen.getByRole('button', { name: cs('test') }));
    expect(await screen.findByRole('status')).toHaveTextContent(cs('testLegacy'));
    expect(screen.getByRole('status')).toHaveTextContent('4.0.27');
  });

  it('a latin1 default from the probe points to 호환성 > 서버 인코딩 until an encoding is chosen', async () => {
    useConnectionsStore.setState({
      connections: [...useConnectionsStore.getState().connections.filter((c) => c.id !== 'c-shop'), { ...oldMysql, host: 'latin1-db.internal' }],
    });
    const user = userEvent.setup();
    renderDialog('c-shop');
    expect(compatToggle()).toHaveAttribute('aria-expanded', 'false');
    await user.type(screen.getByLabelText(cs('password')), 'pw');
    await user.click(screen.getByRole('button', { name: cs('test') }));

    const section = screen.getByRole('region', { name: cs('compat') });
    const notice = await within(section).findByText(cs('encodingNote.latin1Default'));
    expect(within(section).getByText(cs('encodingNoteTitle'))).toBeInTheDocument();
    expect(section).toHaveTextContent('SET NAMES utf8mb4');
    expect(screen.getByText(new RegExp(cs('testEncodingNote')))).toBeInTheDocument();

    await user.click(within(section).getByRole('button', { name: cs('encodingNoteAction') }));
    expect(compatToggle()).toHaveAttribute('aria-expanded', 'true');
    const encoding = screen.getByRole('combobox', { name: cs('field.encoding') });
    expect(encoding).toHaveFocus();

    await choose(user, 'encoding', cs('choice.encoding.euc-kr'));
    expect(notice).not.toBeInTheDocument();
    expect(screen.queryByText(new RegExp(cs('testEncodingNote')))).toBeNull();
  });
});
