import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { TableRef } from 'ipc-types';
import { useState } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { catalogExactCount, catalogLastExactCount } from '@/lib/commands';
import { setConfirmHandler } from '@/lib/confirm';
import { formatDateTime } from '@/lib/format';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import type { StructureSegment } from '@/stores/tabs';
import { useTabsStore } from '@/stores/tabs';
import { resetStores } from '@/test/stores';
import { AppEvents } from '@/components/AppEvents';
import { TooltipProvider } from '@/components/ui';
import { StructureTabView } from './StructureTabView';
import { ts } from './strings';
import { shortDateTime } from './table-ui';

// The real commands (over the mock backend), with `catalogExactCount` and
// `catalogLastExactCount` wrapped so a test can delay their responses.
vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, catalogExactCount: vi.fn(actual.catalogExactCount), catalogLastExactCount: vi.fn(actual.catalogLastExactCount) };
});

const ref: TableRef = { database: 'orders', schema: 'public', name: 'orders' };
const other: TableRef = { database: 'orders', schema: 'public', name: 'customers' };
const key = 'c-prod/orders/public/orders';

/** A count stored by an earlier `catalog_exact_count` job (an earlier session, say). */
const stored = { table: ref, rows: BigInt(1_234_567), elapsedMs: BigInt(1500), at: '2026-10-20T03:04:05.000Z' };

function Harness({ table = ref, initial = 'columns', onChange }: { table?: TableRef; initial?: StructureSegment; onChange?: (s: StructureSegment) => void }) {
  const [segment, setSegment] = useState<StructureSegment>(initial);
  return (
    <StructureTabView
      tabId="t1"
      connId="c-prod"
      table={table}
      segment={segment}
      onSegmentChange={(s) => {
        onChange?.(s);
        setSegment(s);
      }}
    />
  );
}

function renderView(opts: { table?: TableRef; initial?: StructureSegment; events?: boolean; onChange?: (s: StructureSegment) => void } = {}) {
  const { events = false, ...rest } = opts;
  return render(
    <TooltipProvider>
      {events && <AppEvents />}
      <Harness {...rest} />
    </TooltipProvider>,
  );
}

const header = () => screen.getByRole('banner');
const headerCount = () => within(header()).getByRole('button', { name: new RegExp(`${ts('countExactly')}|${ts('counting')}`) });

describe('StructureTabView', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    vi.mocked(catalogExactCount).mockClear();
    vi.mocked(catalogLastExactCount).mockClear();
    mockBackend.configure({ latencyMs: 0, exactCountMs: 30 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
    // The sidebar has loaded this schema, so the estimate is known.
    await useCatalogStore.getState().loadTables('c-prod', 'orders', 'public');
    // `orders` is estimated at 48.2M rows: counting it asks first.
    setConfirmHandler(async () => true);
  });

  it('shows the columns in a dense grid with names, types and key chips; metadata only', async () => {
    renderView();
    const grid = await screen.findByRole('grid', { name: ts('columnsGrid', { table: 'public.orders' }) });
    expect(within(grid).getByText('customer_id')).toBeInTheDocument();
    expect(within(grid).getAllByRole('img', { name: ts('primaryKey') }).length).toBeGreaterThan(0);
    expect(within(grid).getAllByText('PK').length).toBeGreaterThan(0);
    // The header names the table, its kind and database.
    expect(within(header()).getByText('orders')).toBeInTheDocument();
    expect(within(header()).getByText(ts('kind.table'))).toBeInTheDocument();
    expect(within(header()).getByText('@ orders')).toBeInTheDocument();
    // Metadata only: nothing in this view ever asks for table data.
    expect(mockBackend.calls.filter((c) => !c.startsWith('catalog_'))).toEqual([]);
  });

  it('shows indexes with their partial predicate', async () => {
    renderView({ initial: 'indexes' });
    const grid = await screen.findByRole('grid', { name: ts('indexesGrid', { table: 'public.orders' }) });
    expect(within(grid).getByText('idx_orders_open')).toBeInTheDocument();
    const predicate = within(grid).getByText("WHERE (status <> 'cancelled'::text)");
    expect(predicate).toBeInTheDocument();
    // DESIGN.md §3.3: column names are sans everywhere (the 열 and 포함 lists too); mono is for
    // the condition, types and defaults only.
    const cells = within(within(grid).getByText('idx_orders_open').closest('tr') as HTMLElement).getAllByRole('gridcell');
    const headers = within(grid).getAllByRole('columnheader').map((h) => h.textContent);
    expect(cells[headers.indexOf(ts('idx.columns'))]).not.toHaveClass('font-mono');
    expect(cells[headers.indexOf(ts('idx.include'))]).not.toHaveClass('font-mono');
    expect(predicate).toHaveClass('font-mono');
  });

  it('loads the details again after a catalog refresh drops them, in every segment', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByRole('grid', { name: ts('columnsGrid', { table: 'public.orders' }) });
    const columnsCalls = () => mockBackend.calls.filter((c) => c === 'catalog_columns').length;
    const before = columnsCalls();
    // Mod+R / F5, the toolbar, the sidebar or a saved connection edit: `clearConnection` inside.
    await act(async () => {
      await useCatalogStore.getState().refresh('c-prod');
    });
    expect(await screen.findByRole('grid', { name: ts('columnsGrid', { table: 'public.orders' }) })).toBeInTheDocument();
    expect(columnsCalls()).toBe(before + 1);
    await waitFor(() => expect(useCatalogStore.getState().details[key]?.ddl).toBeDefined());
    // The other segments have their data too, not a blank panel.
    await user.click(screen.getByRole('tab', { name: ts('segIndexes') }));
    expect(screen.getByRole('grid', { name: ts('indexesGrid', { table: 'public.orders' }) })).toBeInTheDocument();
    await user.click(screen.getByRole('tab', { name: ts('segDdl') }));
    expect(screen.getByLabelText(ts('ddlLabel', { table: 'public.orders' })).textContent).toMatch(/CREATE TABLE/);
    // Exactly one reload: the details are back, so nothing loads again.
    await act(async () => {});
    expect(columnsCalls()).toBe(before + 1);
  });

  it('does not retry in a loop when the reload after a refresh fails', async () => {
    const missing: TableRef = { database: 'orders', schema: 'public', name: 'missing' };
    renderView({ table: missing });
    await screen.findByRole('alert');
    const columnsCalls = () => mockBackend.calls.filter((c) => c === 'catalog_columns').length;
    const before = columnsCalls();
    await act(async () => {
      await useCatalogStore.getState().refresh('c-prod');
    });
    // The refresh cleared the error, so the view tried once more, and stopped at the new error.
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    await act(async () => {});
    expect(columnsCalls()).toBe(before + 1);
  });

  it('shows the DDL in a code block with a copy button', async () => {
    const user = userEvent.setup();
    renderView({ initial: 'ddl' });
    const block = await screen.findByLabelText(ts('ddlLabel', { table: 'public.orders' }));
    expect(block.textContent).toMatch(/CREATE TABLE/);
    await user.click(screen.getByRole('button', { name: ts('copyDdl') }));
    expect(await navigator.clipboard.readText()).toMatch(/CREATE TABLE/);
  });

  it('switches segments with the arrow keys', async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    renderView({ onChange });
    await screen.findByRole('grid');
    const tabs = screen.getByRole('tablist', { name: ts('sections') });
    within(tabs).getByRole('tab', { name: ts('segColumns') }).focus();
    await user.keyboard('{ArrowRight}');
    expect(onChange).toHaveBeenLastCalledWith('indexes');
    expect(await screen.findByRole('grid', { name: ts('indexesGrid', { table: 'public.orders' }) })).toBeInTheDocument();
    await user.keyboard('{End}');
    expect(onChange).toHaveBeenLastCalledWith('overview');
    expect(screen.getByRole('tabpanel', { name: ts('segOverview') })).toBeInTheDocument();
  });

  it('moves the row selection with the keyboard and copies the selected name with Mod+C', async () => {
    const user = userEvent.setup();
    renderView();
    const grid = await screen.findByRole('grid');
    grid.focus();
    await user.keyboard('{ArrowDown}');
    const rows = within(grid).getAllByRole('row').slice(1);
    expect(rows[0]).toHaveAttribute('aria-selected', 'true');
    expect(grid).toHaveAttribute('aria-activedescendant', rows[0].id);
    await user.keyboard('{ArrowDown}');
    expect(rows[1]).toHaveAttribute('aria-selected', 'true');
    expect(rows[0]).toHaveAttribute('aria-selected', 'false');
    await user.keyboard('{End}');
    expect(rows[rows.length - 1]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{Home}');
    await user.keyboard('{Control>}c{/Control}');
    expect(await navigator.clipboard.readText()).toBe(within(rows[0]).getAllByRole('gridcell')[1].textContent);
  });

  it('copies the column list from the header', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByRole('grid');
    await user.click(screen.getByRole('button', { name: ts('copyColumns') }));
    const names = useCatalogStore.getState().details[key].columns?.map((c) => c.name).join(', ');
    expect(await navigator.clipboard.readText()).toBe(names);
  });

  it('labels the estimate in the overview and offers a count while nothing was counted', async () => {
    renderView({ initial: 'overview' });
    const panel = screen.getByRole('tabpanel', { name: ts('segOverview') });
    expect(within(panel).getByText('≈ 48,200,000 (추정)')).toBeInTheDocument();
    expect(within(panel).getByLabelText(ts('ov.notCounted'))).toBeInTheDocument();
    expect(within(panel).getByRole('button', { name: ts('countExactly') })).toBeEnabled();
    // Column and index counts come from the loaded details.
    await waitFor(() => expect(useCatalogStore.getState().details[key]?.columns).toBeDefined());
    const cols = useCatalogStore.getState().details[key].columns?.length ?? 0;
    await waitFor(() => expect(within(panel).getByText(ts('ov.columns')).nextElementSibling).toHaveTextContent(String(cols)));
  });

  it('counts exactly through a job after the confirm and shows the result when job:done arrives', async () => {
    const user = userEvent.setup();
    const confirm = vi.fn(async () => true);
    setConfirmHandler(confirm);
    renderView({ initial: 'overview', events: true });
    await user.click(headerCount());
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(mockBackend.calls).toContain('catalog_exact_count');
    await waitFor(() => expect(useCatalogStore.getState().exactCounts[key]).toBeDefined(), { timeout: 3000 });
    await waitFor(() => expect(useCatalogStore.getState().counting[key]).toBeUndefined());
    const count = useCatalogStore.getState().exactCounts[key];
    const panel = screen.getByRole('tabpanel', { name: ts('segOverview') });
    expect(await within(panel).findByText(Number(count.rows).toLocaleString('en-US'))).toBeInTheDocument();
    expect(headerCount()).toBeEnabled();
  });

  it('starts no job when the confirm is declined', async () => {
    const user = userEvent.setup();
    setConfirmHandler(async () => false);
    renderView({ initial: 'overview' });
    await user.click(headerCount());
    await act(async () => {});
    expect(vi.mocked(catalogExactCount)).not.toHaveBeenCalled();
    expect(useCatalogStore.getState().counting[key]).toBeUndefined();
  });

  it('does not stay in "counting" when the count finishes before its invoke resolves', async () => {
    const user = userEvent.setup();
    // A small table on a fast server: every `job:*` event, `job:done` included, is handled
    // before the invoke that started the count hands its job id back.
    const actual = await vi.importActual<typeof import('@/lib/commands')>('@/lib/commands');
    vi.mocked(catalogExactCount).mockImplementationOnce(async (connId, table) => {
      const jobId = await actual.catalogExactCount(connId, table);
      await vi.waitFor(() => expect(useCatalogStore.getState().exactCounts[key]).toBeDefined(), { timeout: 3000 });
      return jobId;
    });
    renderView({ initial: 'overview', events: true });
    await user.click(headerCount());
    await act(async () => {
      await vi.waitFor(() => expect(vi.mocked(catalogExactCount).mock.results).toHaveLength(1));
      await vi.mocked(catalogExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().counting[key]).toBeUndefined();
    expect(within(header()).queryByText(ts('counting'))).not.toBeInTheDocument();
    expect(headerCount()).toBeEnabled();
    const count = useCatalogStore.getState().exactCounts[key];
    expect(screen.getByText(Number(count.rows).toLocaleString('en-US'))).toBeInTheDocument();
  });

  it('shows the stored exact count with its date next to the estimate', async () => {
    mockBackend.seedExactCount('c-prod', stored);
    renderView({ initial: 'overview' });
    expect(await screen.findByText('1,234,567')).toBeInTheDocument();
    expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledWith('c-prod', ref);
    expect(screen.getByText(`· ${shortDateTime(stored.at)}`)).toBeInTheDocument();
    const value = screen.getByLabelText(ts('ov.exactAria', { rows: '1,234,567', at: formatDateTime(stored.at, true) }));
    expect(value).toBeInTheDocument();
    // The estimate keeps its label; no second count button while a count is shown.
    expect(screen.getByText('≈ 48,200,000 (추정)')).toBeInTheDocument();
    expect(within(screen.getByRole('tabpanel')).queryByRole('button', { name: ts('countExactly') })).not.toBeInTheDocument();
    // Reading the stored count is metadata only and never starts a job.
    expect(mockBackend.calls).not.toContain('catalog_exact_count');
  });

  it('shows no exact count for a table that was never counted', async () => {
    const { container } = renderView({ initial: 'overview' });
    await waitFor(() => expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledTimes(1));
    await act(async () => {
      await vi.mocked(catalogLastExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().exactCounts[key]).toBeUndefined();
    expect(container.querySelector('[data-exact-count]')).toBeNull();
  });

  it('fetches the stored count again when the tab shows another table', async () => {
    mockBackend.seedExactCount('c-prod', { ...stored, table: other, rows: BigInt(7_654_321) });
    const view = renderView({ initial: 'overview' });
    await waitFor(() => expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledTimes(1));
    view.rerender(
      <TooltipProvider>
        <Harness table={other} initial="overview" />
      </TooltipProvider>,
    );
    expect(await screen.findByText('7,654,321')).toBeInTheDocument();
    expect(vi.mocked(catalogLastExactCount)).toHaveBeenLastCalledWith('c-prod', other);
  });

  it('replaces the stored count when a newer job:done arrives', async () => {
    const user = userEvent.setup();
    mockBackend.seedExactCount('c-prod', stored);
    renderView({ initial: 'overview', events: true });
    expect(await screen.findByText('1,234,567')).toBeInTheDocument();
    await user.click(headerCount());
    await waitFor(() => expect(useCatalogStore.getState().exactCounts[key]?.at).not.toBe(stored.at), { timeout: 3000 });
    const fresh = useCatalogStore.getState().exactCounts[key];
    expect(await screen.findByText(Number(fresh.rows).toLocaleString('en-US'))).toBeInTheDocument();
    expect(screen.queryByText('1,234,567')).not.toBeInTheDocument();
    expect(screen.getByText(`· ${shortDateTime(fresh.at)}`)).toBeInTheDocument();
  });

  it('keeps a job:done count that lands while the stored one is still being read', async () => {
    mockBackend.seedExactCount('c-prod', stored);
    const fresh = { table: ref, rows: BigInt(999), elapsedMs: BigInt(10), at: '2026-10-27T09:00:00.000Z' };
    const actual = await vi.importActual<typeof import('@/lib/commands')>('@/lib/commands');
    // The read resolves only after a newer count went into the store (via job:done).
    vi.mocked(catalogLastExactCount).mockImplementationOnce(async (connId, table) => {
      const old = await actual.catalogLastExactCount(connId, table);
      useCatalogStore.getState().setExactCount('c-prod', fresh);
      return old;
    });
    renderView({ initial: 'overview' });
    await act(async () => {
      await vi.waitFor(() => expect(vi.mocked(catalogLastExactCount).mock.results).toHaveLength(1));
      await vi.mocked(catalogLastExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().exactCounts[key]).toEqual(fresh);
    expect(screen.getByText('999')).toBeInTheDocument();
    expect(screen.queryByText('1,234,567')).not.toBeInTheDocument();
  });

  it('opens a new query tab with the select-all SQL from the overview', async () => {
    const user = userEvent.setup();
    renderView({ initial: 'overview' });
    await user.click(screen.getByRole('button', { name: ts('ov.openQueryTab') }));
    await waitFor(() => expect(useTabsStore.getState().sets['c-prod']?.ids.length).toBe(1));
    const id = useTabsStore.getState().sets['c-prod'].ids[0];
    expect(useTabsStore.getState().tabs[id]).toMatchObject({ kind: 'query', connId: 'c-prod' });
    const doc = useExtractStore.getState().docs[id];
    expect(doc.name).toBe('orders');
    expect(doc.sql).toMatch(/FROM "public"\."orders"/);
    // A fresh tab from a generated query is not dirty.
    expect(doc.baseline).toBe(doc.sql);
  });

  it('shows the extraction query in this table tab from the header', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByRole('grid');
    await user.click(within(header()).getByRole('button', { name: ts('makeQuery') }));
    await waitFor(() => {
      const tab = Object.values(useTabsStore.getState().tabs).find((t) => t.kind === 'table');
      expect(tab).toMatchObject({ kind: 'table', view: 'query', preview: false });
    });
  });

  it('disables the query actions for a table outside the execution database (PostgreSQL)', async () => {
    const browseOnly: TableRef = { database: 'analytics_dw', schema: 'public', name: 'daily_sales' };
    renderView({ table: browseOnly, initial: 'overview' });
    expect(within(header()).getByRole('button', { name: ts('makeQuery') })).toBeDisabled();
    expect(screen.getByRole('button', { name: ts('ov.openQueryTab') })).toBeDisabled();
    // Counting stays possible: it runs where the table is.
    expect(headerCount()).toBeEnabled();
    await waitFor(() => expect(useCatalogStore.getState().details['c-prod/analytics_dw/public/daily_sales']).toBeDefined());
    await waitFor(() => expect(useCatalogStore.getState().loading).toEqual({}));
  });

  it('shows a load error with a retry', async () => {
    const user = userEvent.setup();
    renderView({ table: { database: 'orders', schema: 'public', name: 'missing' } });
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/not found/i);
    const before = mockBackend.calls.filter((c) => c === 'catalog_columns').length;
    await user.click(within(alert).getByRole('button', { name: ts('retry') }));
    await waitFor(() => expect(mockBackend.calls.filter((c) => c === 'catalog_columns').length).toBe(before + 1));
  });

  it('selects the row under the pointer for its context menu', async () => {
    renderView();
    const grid = await screen.findByRole('grid');
    const rows = within(grid).getAllByRole('row').slice(1);
    fireEvent.contextMenu(rows[2]);
    expect(rows[2]).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByRole('menuitem', { name: new RegExp(ts('copyName')) })).toBeInTheDocument();
  });
});
