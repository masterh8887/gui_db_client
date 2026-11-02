import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { TableInfo } from 'ipc-types';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { getAction, registerBuiltinActions } from '@/lib/actions';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { schemaKey, useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWindowsStore, wins } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { __resetPaletteForTests, CommandPalette } from './CommandPalette';
import { ps } from './strings';

const TABLES: TableInfo[] = [
  { schema: 'public', name: 'orders', kind: 'table', estimatedRows: BigInt(48_200_000) },
  { schema: 'public', name: 'order_items', kind: 'table', estimatedRows: BigInt(120_000_000) },
  { schema: 'public', name: 'customers', kind: 'table', estimatedRows: BigInt(1_200_000) },
];

function setupWorkspace({ withTables = true } = {}) {
  useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  useWorkspaceStore.setState({
    mode: 'workspace',
    openConnId: 'c-prod',
    opened: ['c-prod'],
    scope: { 'c-prod': { database: 'orders', schema: 'public' } },
  });
  useTabsStore.setState({ workspaceConnId: 'c-prod' });
  if (withTables) useCatalogStore.setState({ tables: { [schemaKey('c-prod', 'orders', 'public')]: TABLES } });
}

function renderPalette() {
  return render(
    <TooltipProvider>
      <CommandPalette />
    </TooltipProvider>,
  );
}

const input = () => screen.getByRole('combobox', { name: ps('title') });
const options = () => screen.queryAllByRole('option');
const selected = () => options().find((o) => o.getAttribute('aria-selected') === 'true');

describe('CommandPalette', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.configure({ latencyMs: 0 });
    registerBuiltinActions();
  });
  afterEach(() => __resetPaletteForTests());

  it('renders nothing while closed and focuses the input when opened', () => {
    renderPalette();
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    act(() => useUiStore.getState().openPalette());
    expect(screen.getByRole('dialog', { name: ps('title') })).toBeInTheDocument();
    expect(input()).toHaveFocus();
  });

  it('lists tables with their estimate labelled 추정 and opens one as a pinned tab', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette());
    await user.type(input(), 'orders');
    const first = options()[0];
    expect(first).toHaveAttribute('data-kind', 'table');
    expect(within(first).getByText('≈48.2M 추정')).toBeInTheDocument();
    expect(within(first).getByLabelText(/추정/)).toBeInTheDocument();
    // The selected table row shows the new-tab hint.
    expect(within(first).getByText(ps('newTab'))).toBeInTheDocument();
    await user.keyboard('{Enter}');
    expect(useUiStore.getState().palette.open).toBe(false);
    const set = useTabsStore.getState().sets['c-prod'];
    const tab = useTabsStore.getState().tabs[set.activeId!];
    expect(tab).toMatchObject({ kind: 'table', preview: false, table: { schema: 'public', name: 'orders' } });
    expect(useWorkspaceStore.getState().selection['c-prod']).toMatchObject({ name: 'orders' });
  });

  it('opens a table in a new tab with Mod+Enter', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    for (let i = 0; i < 2; i += 1) {
      act(() => useUiStore.getState().openPalette('tables'));
      await user.type(input(), 'customers');
      await user.keyboard('{Control>}{Enter}{/Control}');
    }
    const set = useTabsStore.getState().sets['c-prod'];
    const tables = set.ids.map((id) => useTabsStore.getState().tabs[id]).filter((t) => t.kind === 'table');
    expect(tables).toHaveLength(2);
  });

  it('loads the tables of the current scope when they are missing', async () => {
    const user = userEvent.setup();
    setupWorkspace({ withTables: false });
    renderPalette();
    act(() => useUiStore.getState().openPalette('tables'));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_tables'));
    await user.type(input(), 'orders');
    await waitFor(() => expect(options().length).toBeGreaterThan(0));
    expect(options()[0]).toHaveAttribute('data-kind', 'table');
  });

  it('runs an action only after the palette closed', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette());
    await user.type(input(), `>${getAction('app.settings')!.title()}`);
    expect(selected()).toHaveAttribute('data-kind', 'action');
    // Synchronous keydown: right after it the palette is closed and the action has not run yet.
    act(() => {
      fireEvent.keyDown(input(), { key: 'Enter' });
    });
    expect(useUiStore.getState().palette.open).toBe(false);
    expect(useUiStore.getState().settingsOpen).toBe(false);
    await waitFor(() => expect(useUiStore.getState().settingsOpen).toBe(true));

    // It is remembered as a recent action for the empty query.
    act(() => useUiStore.getState().openPalette());
    expect(screen.getByText(ps('groupRecentActions'))).toBeInTheDocument();
  });

  it('moves with arrows and Ctrl+N / Ctrl+P, completes with Tab', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette('tables'));
    expect(options().map((o) => o.textContent)).toEqual([
      expect.stringContaining('orders'),
      expect.stringContaining('order_items'),
      expect.stringContaining('customers'),
    ]);
    expect(input()).toHaveAttribute('aria-activedescendant', options()[0].id);
    await user.keyboard('{ArrowDown}');
    expect(input()).toHaveAttribute('aria-activedescendant', options()[1].id);
    await user.keyboard('{Control>}n{/Control}');
    expect(selected()).toHaveTextContent('customers');
    await user.keyboard('{Control>}p{/Control}{ArrowUp}{ArrowUp}');
    // Wraps around.
    expect(selected()).toHaveTextContent('customers');
    await user.keyboard('{Tab}');
    expect(input()).toHaveValue('customers');
  });

  it('Esc clears the text first, then closes', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette());
    await user.type(input(), 'ord');
    await user.keyboard('{Escape}');
    expect(input()).toHaveValue('');
    expect(useUiStore.getState().palette.open).toBe(true);
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().palette.open).toBe(false);
  });

  it('shows the scope chip, which Backspace on an empty input removes', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette('connections'));
    expect(screen.getByTestId('palette-scope')).toHaveTextContent(ps('scopeConnections'));
    expect(options().every((o) => o.getAttribute('data-kind') === 'connection')).toBe(true);
    await user.keyboard('{Backspace}');
    expect(screen.queryByTestId('palette-scope')).not.toBeInTheDocument();
  });

  it('switches to connections with Mod+Shift+O; on Windows Ctrl+P stays "previous"', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    renderPalette();
    act(() => useUiStore.getState().openPalette());
    await user.keyboard('{Control>}{Shift>}o{/Shift}{/Control}');
    expect(screen.getByTestId('palette-scope')).toHaveTextContent(ps('scopeConnections'));
    expect(selected()).toBe(options()[0]);
    // Off macOS Mod = Ctrl, and the palette's own Ctrl+P (previous item) wins over the toggle.
    await user.keyboard('{Control>}p{/Control}');
    expect(useUiStore.getState().palette.open).toBe(true);
    expect(selected()).toBe(options().at(-1));
  });

  it('offers connections and actions only on Welcome, and opens a workspace', async () => {
    const user = userEvent.setup();
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
    renderPalette();
    act(() => useUiStore.getState().openPalette());
    expect(input()).toHaveAttribute('placeholder', ps('placeholderWelcome'));
    const kinds = new Set(options().map((o) => o.getAttribute('data-kind')));
    expect([...kinds].sort()).toEqual(['action', 'connection']);
    await user.type(input(), '@staging');
    await user.keyboard('{Enter}');
    await waitFor(() => expect(useWorkspaceStore.getState().openConnId).toBe('c-staging'));
    expect(useWorkspaceStore.getState().mode).toBe('workspace');
  });

  it('pick mode lists the other connections with their tags and re-targets the query', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    const tabId = useTabsStore.getState().openQuery('c-prod', { sql: 'SELECT * FROM public.orders', name: 'orders' });
    renderPalette();
    act(() => useUiStore.getState().openPalette('connections', '', { kind: 'openInConnection', tabId }));
    expect(screen.getByTestId('palette-scope')).toHaveTextContent(ps('scopePick'));
    expect(screen.getByText(ps('groupPick'))).toBeInTheDocument();
    const titles = options().map((o) => o.textContent ?? '');
    expect(titles.some((t) => t.includes('prod (PostgreSQL)'))).toBe(false);
    const staging = options().find((o) => o.textContent?.includes('staging (PostgreSQL)'))!;
    expect(within(staging).getByText('staging')).toBeInTheDocument();
    await user.type(input(), 'staging');
    await user.keyboard('{Enter}');
    await waitFor(() => expect(useWorkspaceStore.getState().openConnId).toBe('c-staging'));
    await waitFor(() => {
      const set = useTabsStore.getState().sets['c-staging'];
      const docs = (set?.ids ?? []).map((id) => useExtractStore.getState().docs[id]).filter(Boolean);
      expect(docs.some((d) => d.sql === 'SELECT * FROM public.orders')).toBe(true);
    });
  });
  it('connection rows carry 창 N chips for other windows (click focuses it); Mod+Enter opens one in a new window', async () => {
    const user = userEvent.setup();
    setupWorkspace();
    useWindowsStore.setState({
      self: { label: 'main', index: 1 },
      windows: [
        { label: 'main', index: 1, title: 'BulkDB — prod (PostgreSQL)', connId: 'c-prod', focused: true },
        { label: 'w-2', index: 2, title: 'BulkDB — staging (PostgreSQL)', connId: 'c-staging', focused: false },
        { label: 'w-3', index: 3, title: 'BulkDB — prod (PostgreSQL)', connId: 'c-prod', focused: false },
      ],
    });
    renderPalette();
    act(() => useUiStore.getState().openPalette('connections'));
    const row = (name: string) => options().find((o) => o.textContent?.includes(name))!;
    // This window is not listed; every other window showing the connection is.
    expect(within(row('staging (PostgreSQL)')).getByRole('button', { name: wins('windowChip', { index: 2 }) })).toBeInTheDocument();
    expect(within(row('prod (PostgreSQL)')).getAllByRole('button').map((b) => b.textContent)).toEqual([wins('windowChip', { index: 3 })]);
    expect(within(row('local (SQLite)')).queryByRole('button')).toBeNull();
    await user.click(within(row('staging (PostgreSQL)')).getByRole('button', { name: wins('windowChip', { index: 2 }) }));
    expect(useUiStore.getState().palette.open).toBe(false);
    await waitFor(() => expect(mockBackend.calls).toContain('window_focus'));
    expect(useWorkspaceStore.getState().openConnId).toBe('c-prod');

    // The selected connection row shows the hint; palette-local Mod+Enter = 새 창에서 열기.
    act(() => useUiStore.getState().openPalette('connections'));
    await user.type(input(), 'local');
    expect(selected()).toHaveTextContent(ps('newWindow'));
    await user.keyboard('{Control>}{Enter}{/Control}');
    expect(useUiStore.getState().palette.open).toBe(false);
    await waitFor(() => expect(mockBackend.calls).toContain('window_new'));
    expect(useWorkspaceStore.getState().openConnId).toBe('c-prod');
  });
});
