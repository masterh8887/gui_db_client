import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { setConfirmHandler } from '@/lib/confirm';
import { mockConnections, mockDatabases, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { selectActiveTab, selectTabList, useTabsStore, type TableTab } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { Sidebar } from './Sidebar';
import { ts } from './strings';

const PROD = 'c-prod';

function renderSidebar(connId = PROD) {
  useWorkspaceStore.setState({ mode: 'workspace', openConnId: connId, opened: [connId] });
  useTabsStore.getState().setWorkspaceConn(connId);
  return render(
    <TooltipProvider>
      <Sidebar connId={connId} />
    </TooltipProvider>,
  );
}

const catalogCalls = () => mockBackend.calls.filter((c) => c.startsWith('catalog_'));
const option = (name: string) => screen.getByRole('option', { name: new RegExp(`^${name},`) });
const findOption = (name: string) => screen.findByRole('option', { name: new RegExp(`^${name},`) });
const list = () => screen.getByRole('listbox', { name: ts('items') });
const tabs = (connId = PROD) => selectTabList(useTabsStore.getState(), connId);
const activeTab = (connId = PROD) => selectActiveTab(useTabsStore.getState(), connId) as TableTab | undefined;
/** Mod+Enter on either platform (`hasModKey` reads metaKey on macOS, ctrlKey elsewhere). */
const modEnter = { key: 'Enter', ctrlKey: true, metaKey: true };

describe('Sidebar', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: PROD, loaded: true });
  });

  it('loads only the browsed schema and labels the row counts of every estimated section', async () => {
    renderSidebar();
    expect(await findOption('orders')).toBeInTheDocument();
    // Lazy: databases, the browsed database's schemas, then only the browsed schema's objects.
    expect(catalogCalls()).toEqual(['catalog_databases', 'catalog_schemas', 'catalog_tables']);
    expect(useWorkspaceStore.getState().scope[PROD]).toEqual({ database: 'orders', schema: 'public' });
    expect(useCatalogStore.getState().tables['c-prod/orders/public']).toHaveLength(15);

    // 15 objects, all tables (one partitioned): one section, captioned 추정.
    const section = screen.getByRole('group', { name: new RegExp(ts('sectionTable')) });
    expect(within(section).getByText('15')).toBeInTheDocument();
    expect(within(section).getByText(ts('estimated'))).toBeInTheDocument();
    expect(option('orders')).toHaveTextContent('≈48.2M');
    expect(option('orders')).toHaveAccessibleName(ts('ariaEstimated', { name: 'orders', rows: '48,200,000' }));

    // Another schema with views and materialized views: every estimated section says 추정.
    act(() => useWorkspaceStore.getState().setScope(PROD, { schema: 'analytics' }));
    expect(await findOption('monthly_sales')).toBeInTheDocument();
    for (const title of [ts('sectionTable'), ts('sectionView'), ts('sectionMatView')]) {
      const group = screen.getByRole('group', { name: new RegExp(`^${title}`) });
      expect(within(group).getByText(ts('estimated'))).toBeInTheDocument();
    }
    expect(screen.getAllByText(ts('estimated'))).toHaveLength(3);
  });

  it('shows an exact count with its date instead of the estimate', async () => {
    renderSidebar();
    await findOption('orders');
    act(() =>
      useCatalogStore.getState().setExactCount(PROD, {
        table: { database: 'orders', schema: 'public', name: 'roles' },
        rows: BigInt(18),
        elapsedMs: BigInt(40),
        at: '2026-10-27T13:02:11',
      }),
    );
    const roles = option('roles');
    expect(roles).toHaveTextContent('18 · 10-27');
    expect(roles).not.toHaveTextContent('≈');
    expect(roles).toHaveAccessibleName(ts('ariaExact', { name: 'roles', rows: '18', at: '2026-10-27 13:02' }));
  });

  it('click opens the preview tab, arrows only move the selection, Enter pins the structure tab', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await user.click(await findOption('orders'));
    expect(activeTab()).toMatchObject({ kind: 'table', preview: true, view: 'structure', table: { database: 'orders', schema: 'public', name: 'orders' } });
    expect(useWorkspaceStore.getState().selection[PROD]).toEqual({ database: 'orders', schema: 'public', name: 'orders' });
    expect(option('orders')).toHaveAttribute('aria-selected', 'true');
    expect(list()).toHaveAttribute('aria-activedescendant', option('orders').id);

    // ↓ ↓ End Home: selection only, no IPC, no new or replaced tab.
    mockBackend.calls.length = 0;
    const before = tabs().map((t) => t.id);
    await user.keyboard('{ArrowDown}');
    expect(useWorkspaceStore.getState().selection[PROD]?.name).toBe('order_items');
    await user.keyboard('{End}');
    expect(useWorkspaceStore.getState().selection[PROD]?.name).toBe('roles');
    await user.keyboard('{Home}{ArrowDown}');
    expect(useWorkspaceStore.getState().selection[PROD]?.name).toBe('order_items');
    expect(option('order_items')).toHaveAttribute('aria-selected', 'true');
    expect(mockBackend.calls).toEqual([]);
    expect(tabs().map((t) => t.id)).toEqual(before);

    await user.keyboard('{Enter}');
    expect(activeTab()).toMatchObject({ kind: 'table', preview: false, view: 'structure', table: { name: 'order_items' } });
    // The preview tab of orders is still there next to the pinned order_items tab.
    expect(tabs().filter((t) => t.kind === 'table')).toHaveLength(2);
  });

  it('double-click pins the row; the preview row is italic', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await user.click(await findOption('customers'));
    expect(within(option('customers')).getByText('customers')).toHaveClass('italic');
    await user.dblClick(option('customers'));
    expect(activeTab()).toMatchObject({ preview: false, table: { name: 'customers' } });
    expect(tabs().filter((t) => t.kind === 'table')).toHaveLength(1);
    expect(within(option('customers')).getByText('customers')).not.toHaveClass('italic');
  });

  it('Mod+Enter opens the select-all query of the selected table and keeps the key from the global Run', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await user.click(await findOption('payments'));
    mockBackend.calls.length = 0;
    const consumed = !fireEvent.keyDown(list(), modEnter);
    expect(consumed).toBe(true);
    await waitFor(() => expect(activeTab()).toMatchObject({ kind: 'table', preview: false, view: 'query', table: { name: 'payments' } }));
    expect(mockBackend.calls).toContain('catalog_select_sql');
  });

  it('refuses the extraction query for a postgres table outside the execution database (탐색 전용)', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    expect(screen.queryByRole('note')).toBeNull();

    act(() => useWorkspaceStore.getState().setScope(PROD, { database: 'analytics_dw' }));
    expect(await findOption('daily_sales')).toBeInTheDocument();
    expect(screen.getByRole('note')).toHaveTextContent(ts('browseOnly', { db: 'orders' }));

    fireEvent.contextMenu(option('daily_sales'));
    const make = await screen.findByRole('menuitem', { name: new RegExp(ts('menuMakeQuery')) });
    expect(make).toHaveAttribute('data-disabled');
    expect(make.getAttribute('title')).toMatch(/orders/);
    await user.keyboard('{Escape}');

    // The keyboard path is refused the same way: a toast with the reason, no query view.
    await user.click(option('daily_sales'));
    fireEvent.keyDown(list(), modEnter);
    await waitFor(() => expect(useToastStore.getState().toasts.some((t) => t.tone === 'danger' && /orders/.test(t.message))).toBe(true));
    expect(activeTab()?.view).toBe('structure');
    expect(mockBackend.calls).not.toContain('catalog_select_sql');
  });

  it('search filters fuzzily, ↓ enters the list, Esc clears, Enter opens the top match', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    const search = screen.getByRole('searchbox', { name: ts('searchLabel') });
    await user.click(search);
    await user.keyboard('itm');
    expect(screen.getAllByRole('option').map((o) => o.textContent?.replace(/≈.*$/, ''))).toEqual(['order_items', 'cart_items']);

    await user.keyboard('{ArrowDown}');
    expect(list()).toHaveFocus();
    expect(useWorkspaceStore.getState().selection[PROD]?.name).toBe('order_items');

    // Esc in the list with an active filter clears it (and is consumed).
    expect(fireEvent.keyDown(list(), { key: 'Escape' })).toBe(false);
    expect(search).toHaveValue('');
    expect(screen.getAllByRole('option')).toHaveLength(15);

    await user.click(search);
    await user.keyboard('zzzq');
    expect(screen.getByText(ts('noMatches', { query: 'zzzq' }))).toBeInTheDocument();
    await user.keyboard('{Escape}');
    expect(search).toHaveValue('');

    await user.keyboard('roles{Enter}');
    expect(activeTab()).toMatchObject({ kind: 'table', preview: false, view: 'structure', table: { name: 'roles' } });
  });

  it('switching workspaces shows the other connection with a fresh filter', async () => {
    const user = userEvent.setup();
    const view = renderSidebar();
    await findOption('orders');
    await user.type(screen.getByRole('searchbox', { name: ts('searchLabel') }), 'ord');
    act(() => useWorkspaceStore.setState({ openConnId: 'c-local', opened: ['c-local', PROD] }));
    view.rerender(
      <TooltipProvider>
        <Sidebar connId="c-local" />
      </TooltipProvider>,
    );
    expect(screen.getByRole('searchbox', { name: ts('searchLabel') })).toHaveValue('');
    expect(await findOption('measurements')).toBeInTheDocument();
    expect(useWorkspaceStore.getState().scope['c-local']).toEqual({ database: 'main', schema: 'main' });
  });

  it('typing in the list moves focus to the search field', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await user.click(await findOption('orders'));
    expect(list()).toHaveFocus();
    fireEvent.keyDown(list(), { key: 'p' });
    expect(screen.getByRole('searchbox', { name: ts('searchLabel') })).toHaveFocus();
  });

  it('collapses a section, persisting it in ui.collapsedSections', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    act(() => useWorkspaceStore.getState().setScope(PROD, { schema: 'analytics' }));
    await findOption('monthly_sales');
    await user.click(screen.getByRole('button', { name: new RegExp(`^${ts('sectionTable')}`) }));
    expect(useUiStore.getState().collapsedSections['items:table']).toBe(true);
    expect(screen.queryByRole('option', { name: /^daily_sales,/ })).toBeNull();
    // Collapsed rows are skipped by the keyboard: ↓ from nothing lands on the first view.
    list().focus();
    await user.keyboard('{ArrowDown}');
    expect(useWorkspaceStore.getState().selection[PROD]?.name).toBe('v_active_customers');
  });

  it('refresh drops the backend metadata cache before reloading what is on screen', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    mockBackend.calls.length = 0;
    await user.click(screen.getByRole('button', { name: ts('refresh') }));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_tables'));
    expect(mockBackend.calls[0]).toBe('catalog_invalidate');
    expect(mockBackend.calls.indexOf('catalog_invalidate')).toBeLessThan(mockBackend.calls.indexOf('catalog_databases'));
    expect(await findOption('orders')).toBeInTheDocument();
    expect(useWorkspaceStore.getState().scope[PROD]).toEqual({ database: 'orders', schema: 'public' });
  });

  it('keeps the refresh spinner with the workspace that started the refresh', async () => {
    const user = userEvent.setup();
    let finish!: () => void;
    const gate = new Promise<void>((resolve) => {
      finish = resolve;
    });
    useCatalogStore.setState({ refresh: () => gate });
    const view = renderSidebar();
    await findOption('orders');
    const refreshButton = () => screen.getByRole('button', { name: ts('refresh') });
    await user.click(refreshButton());
    expect(list()).toHaveAttribute('aria-busy', 'true');
    expect(refreshButton()).toBeDisabled();

    const show = (connId: string) => {
      act(() => useWorkspaceStore.setState({ openConnId: connId, opened: ['c-local', PROD] }));
      view.rerender(
        <TooltipProvider>
          <Sidebar connId={connId} />
        </TooltipProvider>,
      );
    };
    // Another workspace while PROD's refresh is in flight: not busy, its refresh is available.
    show('c-local');
    expect(await findOption('measurements')).toBeInTheDocument();
    expect(list()).not.toHaveAttribute('aria-busy');
    expect(refreshButton()).toBeEnabled();
    // Back on PROD the refresh is still running.
    show(PROD);
    expect(list()).toHaveAttribute('aria-busy', 'true');
    expect(refreshButton()).toBeDisabled();
    await act(async () => finish());
    await waitFor(() => expect(list()).not.toHaveAttribute('aria-busy'));
    expect(refreshButton()).toBeEnabled();
  });

  it('reloads by itself when the cached metadata is dropped elsewhere (connection edited)', async () => {
    renderSidebar();
    await findOption('orders');
    mockBackend.calls.length = 0;
    act(() => useCatalogStore.getState().clearConnection(PROD));
    expect(await findOption('orders')).toBeInTheDocument();
    expect(catalogCalls()).toEqual(expect.arrayContaining(['catalog_databases', 'catalog_tables']));
    expect(catalogCalls()).not.toContain('catalog_invalidate');
  });

  it('row menu: open in a new tab, structure, copy column list and name, count exactly, refresh', async () => {
    const user = userEvent.setup();
    let confirmed = 0;
    setConfirmHandler(async () => {
      confirmed += 1;
      return true;
    });
    renderSidebar();
    await user.click(await findOption('orders'));
    const menu = async () => {
      fireEvent.contextMenu(option('orders'));
      return screen.findByRole('menu');
    };

    const m = await menu();
    expect(within(m).getAllByRole('menuitem').map((i) => i.textContent)).toEqual([
      ts('menuOpenNewTab'),
      expect.stringContaining(ts('menuStructure')),
      expect.stringContaining(ts('menuMakeQuery')),
      ts('menuCount'),
      ts('menuCopyColumns'),
      ts('menuCopyName'),
      ts('menuRefresh'),
    ]);
    await user.click(within(m).getByRole('menuitem', { name: ts('menuOpenNewTab') }));
    expect(activeTab()).toMatchObject({ preview: false, table: { name: 'orders' } });
    expect(tabs().filter((t) => t.kind === 'table')).toHaveLength(2);

    await user.click(within(await menu()).getByRole('menuitem', { name: ts('menuCopyColumns') }));
    await waitFor(async () => expect(await navigator.clipboard.readText()).toMatch(/^id, customer_id, status/));
    expect(mockBackend.calls).toContain('catalog_columns');

    await user.click(within(await menu()).getByRole('menuitem', { name: ts('menuCopyName') }));
    await waitFor(async () => expect(await navigator.clipboard.readText()).toBe('public.orders'));

    // ≥ 10M rows estimated: asks first, then starts the job; the menu says it is counting.
    mockBackend.configure({ exactCountMs: 60_000 });
    await user.click(within(await menu()).getByRole('menuitem', { name: ts('menuCount') }));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_exact_count'));
    expect(confirmed).toBe(1);
    await waitFor(() => expect(Object.keys(useCatalogStore.getState().counting)).toEqual(['c-prod/orders/public/orders']));
    const counting = within(await menu()).getByRole('menuitem', { name: ts('menuCounting') });
    expect(counting).toHaveAttribute('data-disabled');
    await user.keyboard('{Escape}');

    mockBackend.calls.length = 0;
    await user.click(within(await menu()).getByRole('menuitem', { name: ts('menuRefresh') }));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_tables'));
    expect(mockBackend.calls[0]).toBe('catalog_invalidate');
  });

  it('asks for the session password before browsing a connection that asks every time', async () => {
    const user = userEvent.setup();
    useConnectionsStore.setState((s) => ({ connections: s.connections.map((c) => (c.id === PROD ? { ...c, savePassword: false } : c)) }));
    renderSidebar();
    expect(screen.getByText(ts('passwordNeeded'))).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: ts('enterPassword') }));
    expect(useUiStore.getState().dbPasswordPrompts).toMatchObject([{ connId: PROD, requests: [{ reason: 'browse' }] }]);
    expect(catalogCalls()).toEqual([]);

    // Once unlocked, the sidebar loads by itself.
    act(() => useConnectionsStore.setState((s) => ({ unlocked: { ...s.unlocked, [PROD]: true } })));
    expect(await findOption('orders')).toBeInTheDocument();
  });

  it('shows load errors inline with a retry', async () => {
    const user = userEvent.setup();
    // A scope whose schema the server does not know (dropped since it was chosen).
    useCatalogStore.setState({ databases: { [PROD]: mockDatabases(PROD) ?? [] } });
    useWorkspaceStore.setState({ scope: { [PROD]: { database: 'orders', schema: 'nope' } } });
    renderSidebar();
    expect(await screen.findByRole('alert')).toHaveTextContent(/nope/);
    const before = catalogCalls().filter((c) => c === 'catalog_tables').length;
    await user.click(screen.getByRole('button', { name: ts('retry') }));
    await waitFor(() => expect(catalogCalls().filter((c) => c === 'catalog_tables').length).toBe(before + 1));
    expect(await screen.findByRole('alert')).toBeInTheDocument();
  });

  it('shows a database load failure with a retry', async () => {
    const user = userEvent.setup();
    renderSidebar('c-missing');
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: ts('retry') }));
    await waitFor(() => expect(catalogCalls().filter((c) => c === 'catalog_databases')).toHaveLength(2));
  });

  it('shows loading and empty states', async () => {
    mockBackend.configure({ latencyMs: 30 });
    const view = renderSidebar();
    expect(screen.getByText(ts('loading'))).toBeInTheDocument();
    await findOption('orders');
    act(() => useCatalogStore.setState((s) => ({ tables: { ...s.tables, 'c-prod/orders/public': [] } })));
    expect(screen.getByText(ts('noTables'))).toBeInTheDocument();
    act(() => useCatalogStore.setState((s) => ({ schemas: { ...s.schemas, 'c-prod/orders': [] } })));
    expect(screen.getByText(ts('noSchemas'))).toBeInTheDocument();
    act(() => useCatalogStore.setState((s) => ({ databases: { ...s.databases, [PROD]: [] } })));
    expect(screen.getByText(ts('noDatabases'))).toBeInTheDocument();
    view.unmount();
  });

  it('consumes sidebar requests: focusSearch focuses the search, database opens the picker', async () => {
    renderSidebar();
    await findOption('orders');
    act(() => useUiStore.getState().requestSidebar('focusSearch'));
    expect(screen.getByRole('searchbox', { name: ts('searchLabel') })).toHaveFocus();
    expect(useUiStore.getState().sidebarRequest).toBeNull();

    act(() => useUiStore.getState().requestSidebar('database'));
    expect(await screen.findByRole('dialog', { name: ts('pickDatabase') })).toBeInTheDocument();
    expect(useUiStore.getState().sidebarRequest).toBeNull();

    act(() => useUiStore.getState().requestSidebar('schema'));
    expect(await screen.findByRole('dialog', { name: ts('pickSchema') })).toBeInTheDocument();
    expect(screen.queryByRole('dialog', { name: ts('pickDatabase') })).toBeNull();
  });
});

describe('Sidebar database / schema switcher', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: PROD, loaded: true });
  });

  it('lists databases with size and marks, skips inaccessible ones, and searches', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    act(() =>
      useCatalogStore.setState((s) => ({
        databases: { ...s.databases, [PROD]: [...s.databases[PROD], { name: 'secret_hr', accessible: false, isCurrent: false }] },
      })),
    );
    await user.click(screen.getByRole('button', { name: ts('databaseButton', { name: 'orders' }) }));
    const dialog = await screen.findByRole('dialog', { name: ts('pickDatabase') });
    const rows = within(dialog).getAllByRole('option');
    expect(rows.map((r) => r.textContent)).toEqual([
      `orders${ts('current')} ${ts('exec')}48 GB`,
      'analytics_dw120 GB',
      `secret_hr${ts('inaccessible')}`,
    ]);
    expect(rows[0]).toHaveAttribute('aria-current', 'true');
    expect(rows[2]).toHaveAttribute('aria-disabled', 'true');
    await user.click(rows[2]);
    expect(useWorkspaceStore.getState().scope[PROD]?.database).toBe('orders');

    // The search field has focus: type, then Enter picks the highlighted match.
    expect(within(dialog).getByRole('combobox', { name: ts('searchDatabases') })).toHaveFocus();
    await user.keyboard('ana{Enter}');
    expect(screen.queryByRole('dialog')).toBeNull();
    await waitFor(() => expect(useWorkspaceStore.getState().scope[PROD]).toEqual({ database: 'analytics_dw', schema: 'public' }));
    expect(await findOption('daily_sales')).toBeInTheDocument();
  });

  it('switches the schema from a list with table counts (keyboard)', async () => {
    const user = userEvent.setup();
    renderSidebar();
    await findOption('orders');
    await user.click(screen.getByRole('button', { name: ts('schemaButton', { name: 'public' }) }));
    const dialog = await screen.findByRole('dialog', { name: ts('pickSchema') });
    const rows = within(dialog).getAllByRole('option');
    expect(rows.map((r) => r.textContent)).toEqual(['public15', 'analytics15']);
    expect(within(rows[0]).getByLabelText(ts('tableCount', { count: '15' }))).toBeInTheDocument();
    // Focus stays in the search field, which is the combobox naming the highlighted row; the
    // never-focused list carries no active descendant of its own.
    const search = within(dialog).getByRole('combobox', { name: ts('searchSchemas') });
    const listbox = within(dialog).getByRole('listbox', { name: ts('pickSchema') });
    expect(search).toHaveFocus();
    expect(search).toHaveAttribute('aria-controls', listbox.id);
    expect(search).toHaveAttribute('aria-expanded', 'true');
    expect(search).toHaveAttribute('aria-activedescendant', rows[0].id);
    expect(listbox).not.toHaveAttribute('aria-activedescendant');
    await user.keyboard('{ArrowDown}');
    expect(search).toHaveAttribute('aria-activedescendant', rows[1].id);
    expect(rows[1]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('x');
    expect(within(dialog).queryAllByRole('option')).toHaveLength(0);
    expect(search).not.toHaveAttribute('aria-activedescendant');
    await user.keyboard('{Backspace}');
    expect(search).toHaveAttribute('aria-activedescendant', within(dialog).getAllByRole('option')[1].id);
    await user.keyboard('{Enter}');
    expect(useWorkspaceStore.getState().scope[PROD]).toEqual({ database: 'orders', schema: 'analytics' });
    expect(await findOption('monthly_sales')).toBeInTheDocument();
    // Esc closes the picker without a change.
    await user.click(screen.getByRole('button', { name: ts('schemaButton', { name: 'analytics' }) }));
    await screen.findByRole('dialog', { name: ts('pickSchema') });
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(useWorkspaceStore.getState().scope[PROD]?.schema).toBe('analytics');
  });
});
