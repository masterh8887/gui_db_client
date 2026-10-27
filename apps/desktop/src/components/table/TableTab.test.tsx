import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { catalogExactCount, catalogLastExactCount } from '@/lib/commands';
import { formatDateTime } from '@/lib/format';
import { t } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { AppEvents } from '@/components/AppEvents';
import { TooltipProvider } from '@/components/ui';
import { TableTab } from './TableTab';

// The real commands (over the mock backend), with `catalogExactCount` and
// `catalogLastExactCount` wrapped so a test can delay their responses.
vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, catalogExactCount: vi.fn(actual.catalogExactCount), catalogLastExactCount: vi.fn(actual.catalogLastExactCount) };
});

const ref = { database: 'orders', schema: 'public', name: 'orders' };
const key = 'c-prod/orders/public/orders';

/** A count stored by an earlier `catalog_exact_count` job (an earlier session, say). */
const stored = { table: ref, rows: BigInt(1_234_567), elapsedMs: BigInt(1500), at: '2026-10-20T03:04:05.000Z' };

/** Matches any "exact (time)" label, whatever the time. */
const exactLabel = (content: string) => content.startsWith(t('table.exactAt', { at: '@' }).split('@')[0]);

function renderTab(withEvents = false) {
  return render(
    <TooltipProvider>
      {withEvents && <AppEvents />}
      <TableTab />
    </TooltipProvider>,
  );
}

describe('TableTab', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    vi.mocked(catalogExactCount).mockClear();
    vi.mocked(catalogLastExactCount).mockClear();
    mockBackend.configure({ latencyMs: 0, exactCountMs: 30 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
    // The tree has loaded this schema, so the table's estimate is known.
    await useCatalogStore.getState().loadTables('c-prod', 'orders', 'public');
    useUiStore.getState().selectTable({ connId: 'c-prod', ref });
  });

  it('shows columns, indexes with their partial predicate, and the DDL; rows are estimates', async () => {
    render(
      <TooltipProvider>
        <TableTab />
      </TooltipProvider>,
    );
    // `customer_id` is both a column and the column list of an index.
    expect((await screen.findAllByText('customer_id')).length).toBe(2);
    expect(screen.getByText('idx_orders_open')).toBeInTheDocument();
    expect(screen.getByText("WHERE (status <> 'cancelled'::text)")).toBeInTheDocument();
    expect(screen.getByText(/CREATE TABLE/)).toBeInTheDocument();
    expect(screen.getAllByText(t('estimated')).length).toBeGreaterThan(0);
    // Metadata only: nothing in this tab ever asks for table data.
    expect(mockBackend.calls.filter((c) => !c.startsWith('catalog_'))).toEqual([]);
  });

  it('counts exactly through a job and shows the result when job:done arrives', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <AppEvents />
        <TableTab />
      </TooltipProvider>,
    );
    await user.click(await screen.findByRole('button', { name: new RegExp(t('table.countExact')) }));
    expect(mockBackend.calls).toContain('catalog_exact_count');
    await waitFor(() => expect(screen.queryByText(t('table.counting'))).not.toBeInTheDocument(), { timeout: 3000 });
    const count = useCatalogStore.getState().exactCounts['c-prod/orders/public/orders'];
    expect(count).toBeDefined();
    expect(screen.getByText(Number(count.rows).toLocaleString('en-US'))).toBeInTheDocument();
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
    render(
      <TooltipProvider>
        <AppEvents />
        <TableTab />
      </TooltipProvider>,
    );
    await user.click(await screen.findByRole('button', { name: new RegExp(t('table.countExact')) }));
    // Let the click handler continue past the (late) response before looking.
    await act(async () => {
      await vi.mocked(catalogExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().counting[key]).toBeUndefined();
    expect(screen.queryByText(t('table.counting'))).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: new RegExp(t('table.countExact')) })).toBeEnabled();
    const count = useCatalogStore.getState().exactCounts[key];
    expect(screen.getByText(Number(count.rows).toLocaleString('en-US'))).toBeInTheDocument();
  });

  it('shows the stored exact count with its time next to the estimate when the table is selected', async () => {
    mockBackend.seedExactCount('c-prod', stored);
    renderTab();
    expect(await screen.findByText('1,234,567')).toBeInTheDocument();
    expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledWith('c-prod', ref);
    // The estimate keeps its "estimated" label; the exact count says when it was taken.
    expect(screen.getAllByText(t('estimated')).length).toBeGreaterThan(0);
    const label = screen.getByText(t('table.exactAt', { at: formatDateTime(stored.at) }));
    expect(label).toHaveAttribute('title', expect.stringContaining(formatDateTime(stored.at, true)));
    // Reading the stored count is metadata only and never starts a job.
    expect(mockBackend.calls).not.toContain('catalog_exact_count');
  });

  it('shows no exact count for a table that was never counted', async () => {
    renderTab();
    await waitFor(() => expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledTimes(1));
    await act(async () => {
      await vi.mocked(catalogLastExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().exactCounts[key]).toBeUndefined();
    expect(screen.queryByText(exactLabel)).not.toBeInTheDocument();
  });

  it('fetches the stored count again for every selected table', async () => {
    const other = { database: 'orders', schema: 'public', name: 'customers' };
    mockBackend.seedExactCount('c-prod', { ...stored, table: other, rows: BigInt(7_654_321) });
    renderTab();
    await waitFor(() => expect(vi.mocked(catalogLastExactCount)).toHaveBeenCalledTimes(1));
    act(() => useUiStore.getState().selectTable({ connId: 'c-prod', ref: other }));
    expect(await screen.findByText('7,654,321')).toBeInTheDocument();
    expect(screen.queryByText('1,234,567')).not.toBeInTheDocument();
    expect(vi.mocked(catalogLastExactCount)).toHaveBeenLastCalledWith('c-prod', other);
  });

  it('replaces the stored count when a newer job:done arrives', async () => {
    const user = userEvent.setup();
    mockBackend.seedExactCount('c-prod', stored);
    renderTab(true);
    expect(await screen.findByText('1,234,567')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: new RegExp(t('table.countExact')) }));
    await waitFor(() => expect(useCatalogStore.getState().exactCounts[key]?.at).not.toBe(stored.at), { timeout: 3000 });
    const fresh = useCatalogStore.getState().exactCounts[key];
    expect(await screen.findByText(Number(fresh.rows).toLocaleString('en-US'))).toBeInTheDocument();
    expect(screen.queryByText('1,234,567')).not.toBeInTheDocument();
    expect(screen.getByText(t('table.exactAt', { at: formatDateTime(fresh.at) }))).toBeInTheDocument();
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
    renderTab();
    await act(async () => {
      await vi.waitFor(() => expect(vi.mocked(catalogLastExactCount).mock.results).toHaveLength(1));
      await vi.mocked(catalogLastExactCount).mock.results[0].value;
    });
    expect(useCatalogStore.getState().exactCounts[key]).toEqual(fresh);
    expect(screen.getByText('999')).toBeInTheDocument();
    expect(screen.queryByText('1,234,567')).not.toBeInTheDocument();
  });
});
