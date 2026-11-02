import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { TableRef } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { formatBytes } from '@/lib/format';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useTabsStore } from '@/stores/tabs';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ts } from './strings';
import { TableInspector } from './TableInspector';

const ref: TableRef = { database: 'orders', schema: 'public', name: 'orders' };
const key = 'c-prod/orders/public/orders';
const stored = { table: ref, rows: BigInt(1_234_567), elapsedMs: BigInt(1500), at: '2026-10-20T03:04:05.000Z' };

function renderInspector(source: 'tab' | 'selection', table: TableRef | null = ref) {
  return render(
    <TooltipProvider>
      <TableInspector connId="c-prod" table={table} source={source} />
    </TooltipProvider>,
  );
}

describe('TableInspector', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
    await useCatalogStore.getState().loadTables('c-prod', 'orders', 'public');
    mockBackend.calls.length = 0;
  });

  it('shows the sidebar selection from the catalog store alone, without any IPC call', async () => {
    renderInspector('selection');
    const info = useCatalogStore.getState().findTable('c-prod', ref);
    expect(screen.getByText('≈ 48,200,000 (추정)')).toBeInTheDocument();
    expect(screen.getByText(formatBytes(info?.totalBytes))).toBeInTheDocument();
    expect(screen.getByText('One row per order')).toBeInTheDocument();
    expect(screen.getByText(ts('insp.notCached'))).toBeInTheDocument();
    await act(async () => {});
    expect(mockBackend.calls).toEqual([]);
  });

  it('lists cached columns and a stored exact count for the selection, still without IPC', async () => {
    await useCatalogStore.getState().loadTableDetails('c-prod', ref);
    useCatalogStore.getState().setExactCount('c-prod', stored);
    mockBackend.calls.length = 0;
    renderInspector('selection');
    expect(screen.getByRole('list', { name: ts('insp.columns') })).toHaveTextContent('customer_id');
    expect(screen.getByText('1,234,567')).toBeInTheDocument();
    await act(async () => {});
    expect(mockBackend.calls).toEqual([]);
  });

  it('loads the details and the stored count for the active table tab', async () => {
    mockBackend.seedExactCount('c-prod', stored);
    renderInspector('tab');
    expect(await screen.findByRole('list', { name: ts('insp.columns') })).toHaveTextContent('customer_id');
    expect(await screen.findByText('1,234,567')).toBeInTheDocument();
    expect(mockBackend.calls).toEqual(expect.arrayContaining(['catalog_columns', 'catalog_indexes', 'catalog_ddl', 'catalog_last_exact_count']));
    expect(useCatalogStore.getState().details[key]?.indexes).toBeDefined();
  });

  it('loads the active tab details again after a catalog refresh drops them', async () => {
    renderInspector('tab');
    await screen.findByRole('list', { name: ts('insp.columns') });
    mockBackend.calls.length = 0;
    await act(async () => {
      await useCatalogStore.getState().refresh('c-prod');
    });
    expect(await screen.findByRole('list', { name: ts('insp.columns') })).toHaveTextContent('customer_id');
    expect(await screen.findByRole('list', { name: ts('insp.indexes') })).toBeInTheDocument();
    expect(mockBackend.calls.filter((c) => c === 'catalog_columns')).toHaveLength(1);
  });

  it('loads nothing for the selection when a refresh drops the cached details', async () => {
    await useCatalogStore.getState().loadTableDetails('c-prod', ref);
    renderInspector('selection');
    expect(screen.getByRole('list', { name: ts('insp.columns') })).toBeInTheDocument();
    mockBackend.calls.length = 0;
    act(() => useCatalogStore.getState().clearConnection('c-prod'));
    expect(screen.getByText(ts('insp.notCached'))).toBeInTheDocument();
    await act(async () => {});
    expect(mockBackend.calls).toEqual([]);
  });

  it('lists index columns in sans like every other object name (DESIGN.md §3.3)', async () => {
    renderInspector('tab');
    const list = await screen.findByRole('list', { name: ts('insp.indexes') });
    expect(list.querySelector('.font-mono')).toBeNull();
  });

  it('opens the selection in a pinned structure tab', async () => {
    const user = userEvent.setup();
    renderInspector('selection');
    await user.click(screen.getByRole('button', { name: ts('insp.open') }));
    await waitFor(() => {
      const tabs = Object.values(useTabsStore.getState().tabs);
      expect(tabs).toEqual([expect.objectContaining({ kind: 'table', preview: false, view: 'structure', table: ref })]);
    });
  });

  it('offers no open button for the active tab (it is already open)', async () => {
    renderInspector('tab');
    expect(screen.queryByRole('button', { name: ts('insp.open') })).not.toBeInTheDocument();
    await screen.findByRole('list', { name: ts('insp.columns') });
  });

  it('says so when no table is selected', () => {
    renderInspector('selection', null);
    expect(screen.getByText(ts('insp.none'))).toBeInTheDocument();
  });
});
