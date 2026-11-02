import type { Connection } from 'ipc-types';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { setConfirmHandler } from '@/lib/confirm';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { ws } from './strings';
import { WelcomeScreen } from './WelcomeScreen';

const oracleConn: Connection = {
  id: 'c-erp',
  name: 'erp (Oracle)',
  driver: 'oracle',
  host: 'erp-db.internal',
  port: 1521,
  fields: { service_name: 'ERP' },
  session: {},
  savePassword: true,
  hasPassword: false,
  hasSshSecret: false,
};

/** Render and wait for `app_info` (the version line), so no update lands after the test. */
async function renderWelcome() {
  render(
    <TooltipProvider>
      <WelcomeScreen headerEnd={<span data-testid="header-end" />} />
    </TooltipProvider>,
  );
  await screen.findByText('v0.1.0-mock');
}

const list = () => screen.getByRole('listbox', { name: ws('listLabel') });
const rowNames = () => within(list()).getAllByRole('option').map((o) => o.querySelector('.font-medium')?.textContent);
const row = (id: string) => list().querySelector<HTMLElement>(`[data-conn-id="${id}"]`)!;
const search = () => screen.getByRole('searchbox', { name: ws('searchLabel') });

describe('WelcomeScreen', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true, selectedId: 'c-prod' });
  });

  it('lists connections by name with tag keys, driver badges, the second line, version and mock chip', async () => {
    await renderWelcome();
    expect(rowNames()).toEqual(['local (SQLite)', 'prod (PostgreSQL)', 'staging (PostgreSQL)']);
    const prod = row('c-prod');
    expect(prod).toHaveAttribute('aria-selected', 'true');
    expect(within(prod).getByText('production')).toBeInTheDocument();
    expect(within(prod).getByText('Pg')).toBeInTheDocument();
    expect(prod).toHaveTextContent('db-prod.internal:5432 · orders · SSH deploy@bastion.example.com');
    // SQLite: the path (middle-truncated, full path in the tooltip).
    expect(within(row('c-local')).getByTitle('C:\\data\\local.db')).toHaveTextContent('C:\\data\\local.db');
    expect(screen.getByTestId('header-end')).toBeInTheDocument();
    expect(screen.getByText('v0.1.0-mock')).toBeInTheDocument();
    expect(screen.getByText(ws('mock'))).toBeInTheDocument();
    // No test ran this session: no status dot.
    expect(prod.querySelector('[data-status]')).toBeNull();
  });

  it('fuzzy search filters and selects the best hit; Esc clears it', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    await user.click(search());
    await user.keyboard('db-staging');
    expect(rowNames()).toEqual(['staging (PostgreSQL)']);
    expect(useConnectionsStore.getState().selectedId).toBe('c-staging');
    // Enter in the field opens the selected hit.
    await user.keyboard('{Enter}');
    expect(useWorkspaceStore.getState().openConnId).toBe('c-staging');

    await user.clear(search());
    await user.type(search(), 'zzzz');
    expect(screen.getByText(ws('noMatch'))).toBeInTheDocument();
    await user.keyboard('{Escape}');
    expect(search()).toHaveValue('');
    expect(rowNames()).toHaveLength(3);
  });

  it('groups by tag (risk order) and by driver from the 정렬 menu', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    await user.click(screen.getByRole('button', { name: ws('sort') }));
    await user.click(await screen.findByRole('menuitem', { name: ws('byTag') }));
    expect(useUiStore.getState().welcomeGroupBy).toBe('tag');
    expect(within(list()).getAllByRole('group').map((g) => g.getAttribute('aria-label'))).toEqual([
      'production — 운영',
      'staging — 스테이징',
      'local — 로컬',
    ]);
    await user.click(screen.getByRole('button', { name: ws('sort') }));
    await user.click(await screen.findByRole('menuitem', { name: ws('byDriver') }));
    expect(within(list()).getAllByRole('group').map((g) => g.getAttribute('aria-label'))).toEqual(['PostgreSQL', 'SQLite']);
  });

  it('arrows move the selection, Enter and double-click open the workspace', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    act(() => list().focus());
    await user.keyboard('{ArrowDown}');
    expect(useConnectionsStore.getState().selectedId).toBe('c-staging');
    await user.keyboard('{Home}');
    expect(useConnectionsStore.getState().selectedId).toBe('c-local');
    await user.keyboard('{Enter}');
    expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: 'c-local' });
    await user.dblClick(row('c-prod'));
    expect(useWorkspaceStore.getState().openConnId).toBe('c-prod');
  });

  it('Delete and Mod+Backspace ask first; declining keeps the connection, accepting selects the next row', async () => {
    const user = userEvent.setup();
    const ask = vi.fn(async () => false);
    setConfirmHandler(ask);
    await renderWelcome();
    act(() => list().focus());
    await user.keyboard('{Delete}');
    expect(ask).toHaveBeenCalledTimes(1);
    expect(rowNames()).toHaveLength(3);

    ask.mockImplementation(async () => true);
    await user.keyboard('{Control>}{Backspace}{/Control}');
    await waitFor(() => expect(rowNames()).toEqual(['local (SQLite)', 'staging (PostgreSQL)']));
    expect(ask).toHaveBeenCalledTimes(2);
    expect(useConnectionsStore.getState().selectedId).toBe('c-staging');
    expect(mockBackend.calls).toContain('conn_delete');
  });

  it('with nothing focused, arrows select; Mod+F and printable keys focus the search', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    expect(document.activeElement).toBe(document.body);
    await user.keyboard('{ArrowDown}');
    expect(useConnectionsStore.getState().selectedId).toBe('c-staging');
    expect(list()).toHaveFocus();

    act(() => (document.activeElement as HTMLElement).blur());
    await user.keyboard('{Control>}f{/Control}');
    expect(search()).toHaveFocus();

    act(() => list().focus());
    fireEvent.keyDown(list(), { key: 'p' });
    expect(search()).toHaveFocus();

    // IME composition never steals focus.
    act(() => list().focus());
    fireEvent.keyDown(list(), { key: 'Process', isComposing: true });
    expect(list()).toHaveFocus();
  });

  it('context menu duplicates (copy selected, toast) and the ⋯ menu tests the connection', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    await user.pointer({ keys: '[MouseRight]', target: row('c-staging') });
    expect(useConnectionsStore.getState().selectedId).toBe('c-staging');
    await user.click(await screen.findByRole('menuitem', { name: ws('menuDuplicate') }));
    await waitFor(() => expect(rowNames()).toHaveLength(4));
    const copy = useConnectionsStore.getState().connections.find((c) => c.name === 'staging (PostgreSQL) 복사본');
    expect(copy).toBeDefined();
    expect(useConnectionsStore.getState().selectedId).toBe(copy!.id);
    expect(useToastStore.getState().toasts.at(-1)?.message).toMatch(/비밀번호는 복사되지 않았습니다/);

    await user.click(within(row('c-staging')).getByRole('button', { name: ws('more') }));
    await user.click(await screen.findByRole('menuitem', { name: ws('menuTest') }));
    await waitFor(() => expect(row('c-staging').querySelector('[data-status="ok"]')).not.toBeNull());
  });

  it('a failed test shows a red dot whose tooltip text is the error', async () => {
    useConnectionsStore.setState({ status: { 'c-staging': 'error' }, errors: { 'c-staging': 'password authentication failed' } });
    await renderWelcome();
    const dot = row('c-staging').querySelector('[data-status="error"]');
    expect(dot).not.toBeNull();
    expect(dot).toHaveAttribute('aria-label', '연결 실패');
  });

  it('rescans drivers from the 정렬 menu and toasts the available count', async () => {
    const user = userEvent.setup();
    mockBackend.installDriver('oracle');
    await renderWelcome();
    await user.click(screen.getByRole('button', { name: ws('sort') }));
    await user.click(await screen.findByRole('menuitem', { name: ws('rescan') }));
    await waitFor(() => expect(mockBackend.calls).toContain('drivers_rescan'));
    await waitFor(() => expect(useToastStore.getState().toasts.at(-1)?.message).toMatch(/6개 사용 가능/));
  });

  it('empty: offers New connection and DBeaver import', async () => {
    const user = userEvent.setup();
    useConnectionsStore.setState({ connections: [], selectedId: null });
    await renderWelcome();
    const empty = screen.getByText(ws('empty')).parentElement!;
    await user.click(within(empty).getByRole('button', { name: ws('newConnection') }));
    expect(useUiStore.getState().connectionDialog).toEqual({ open: true, editId: null });
    await user.click(within(empty).getByRole('button', { name: ws('importDbeaver') }));
    expect(useUiStore.getState().importDbeaverOpen).toBe(true);
  });

  it('open workspaces show 열림 · 탭 N, are listed in the side pane and offer 작업공간 닫기', async () => {
    const user = userEvent.setup();
    await useWorkspaceStore.getState().openWorkspace('c-staging');
    useWorkspaceStore.getState().showWelcome();
    await renderWelcome();
    expect(row('c-staging')).toHaveTextContent(ws('openTab'));
    const side = screen.getByRole('button', { name: /staging \(PostgreSQL\)/ });
    expect(side).toHaveTextContent(ws('tabs', { n: 1 }));
    await user.click(side);
    expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: 'c-staging' });

    await user.pointer({ keys: '[MouseRight]', target: row('c-staging') });
    await user.click(await screen.findByRole('menuitem', { name: ws('menuCloseWorkspace') }));
    await waitFor(() => expect(useWorkspaceStore.getState().opened).not.toContain('c-staging'));
  });

  it('marks a connection whose driver is not installed', async () => {
    useConnectionsStore.setState((s) => ({ connections: [...s.connections, oracleConn] }));
    await renderWelcome();
    expect(within(row('c-erp')).getByText(ws('driverNeeded'))).toBeInTheDocument();
    expect(within(row('c-prod')).queryByText(ws('driverNeeded'))).toBeNull();
  });

  it('settings and About open from the side pane footer', async () => {
    const user = userEvent.setup();
    await renderWelcome();
    await user.click(screen.getByRole('button', { name: ws('settings') }));
    expect(useUiStore.getState().settingsOpen).toBe(true);
    await user.click(screen.getByRole('button', { name: ws('about') }));
    expect(useUiStore.getState().aboutOpen).toBe(true);
  });
});
