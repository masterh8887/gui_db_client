import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { t } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ConnectionTree } from './ConnectionTree';

describe('ConnectionTree', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('expands connection -> database -> schema and labels row counts as estimated', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <ConnectionTree />
      </TooltipProvider>,
    );
    expect(screen.getByText('prod (PostgreSQL)')).toBeInTheDocument();
    expect(screen.getByText('local (SQLite)')).toBeInTheDocument();

    await user.click(screen.getByText('prod (PostgreSQL)'));
    await user.click(await screen.findByText('orders'));
    await user.click(await screen.findByText('public'));

    const estimated = await screen.findAllByText(t('estimated'));
    expect(estimated.length).toBeGreaterThan(0);
    expect(screen.getByText('order_items')).toBeInTheDocument();
    expect(screen.getByText(/48\.2M/)).toBeInTheDocument();
    expect(useCatalogStore.getState().tables['c-prod/orders/public']).toHaveLength(15);
    expect(mockBackend.calls).toEqual(['catalog_databases', 'catalog_schemas', 'catalog_tables']);
  });

  it('imports DBeaver connections from the header menu (without passwords, skipping unsupported drivers)', async () => {
    const user = userEvent.setup();
    const prompt = vi.spyOn(window, 'prompt').mockReturnValue('C:\\Users\\me\\.dbeaver\\data-sources.json');
    try {
      render(
        <TooltipProvider>
          <ConnectionTree />
        </TooltipProvider>,
      );
      await user.click(screen.getByRole('button', { name: t('tree.more') }));
      await user.click(await screen.findByRole('menuitem', { name: t('tree.importDbeaver') }));
      const dialog = await screen.findByRole('dialog', { name: t('import.title') });
      expect(mockBackend.calls).toContain('conn_import_dbeaver');

      // Unsupported provider: shown, but cannot be selected. Vendor driver not installed: kept.
      expect(within(dialog).getByRole('checkbox', { name: 'events-clickhouse (DBeaver)' })).toBeDisabled();
      expect(within(dialog).getByText(t('import.unsupported'))).toBeInTheDocument();
      expect(within(dialog).getByText(t('import.driverMissing'))).toBeInTheDocument();
      expect(within(dialog).getByRole('checkbox', { name: 'erp-oracle (DBeaver)' })).toBeChecked();
      expect(within(dialog).getByRole('checkbox', { name: 'staging-mysql (DBeaver)' })).toBeChecked();

      await user.click(within(dialog).getByRole('button', { name: t('import.save', { count: 2 }) }));
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      const names = useConnectionsStore.getState().connections.map((c) => c.name);
      expect(names).toEqual(expect.arrayContaining(['erp-oracle (DBeaver)', 'staging-mysql (DBeaver)']));
      expect(names).not.toContain('events-clickhouse (DBeaver)');
      const imported = useConnectionsStore.getState().connections.find((c) => c.name === 'staging-mysql (DBeaver)');
      expect(imported?.hasPassword).toBe(false);
      expect(imported?.id).not.toBe('dbeaver-my1');
      // The list was reloaded from the backend after saving.
      expect(mockBackend.calls.filter((c) => c === 'conn_save')).toHaveLength(2);
      expect(mockBackend.calls.lastIndexOf('conn_list')).toBeGreaterThan(mockBackend.calls.lastIndexOf('conn_save'));
      expect(screen.getByText('staging-mysql (DBeaver)')).toBeInTheDocument();
    } finally {
      prompt.mockRestore();
    }
  });

  it('shows why a DBeaver file could not be read', async () => {
    const user = userEvent.setup();
    const prompt = vi.spyOn(window, 'prompt').mockReturnValue('C:\\missing\\data-sources.json');
    try {
      render(
        <TooltipProvider>
          <ConnectionTree />
        </TooltipProvider>,
      );
      await user.click(screen.getByRole('button', { name: t('tree.more') }));
      await user.click(await screen.findByRole('menuitem', { name: t('tree.importDbeaver') }));
      const dialog = await screen.findByRole('dialog', { name: t('import.title') });
      expect(within(dialog).getByText(/no such file/)).toBeInTheDocument();
      expect(within(dialog).getByRole('button', { name: t('import.save', { count: 0 }) })).toBeDisabled();
    } finally {
      prompt.mockRestore();
    }
  });

  it('rescans drivers from the header menu so a newly installed vendor client is picked up', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <ConnectionTree />
      </TooltipProvider>,
    );
    const oracle = () => useConnectionsStore.getState().drivers.find((d) => d.id === 'oracle');
    expect(oracle()?.available).toBe(false);
    mockBackend.installDriver('oracle');
    await user.click(screen.getByRole('button', { name: t('tree.more') }));
    await user.click(await screen.findByRole('menuitem', { name: t('drivers.rescan') }));
    await waitFor(() => expect(oracle()?.available).toBe(true));
    expect(mockBackend.calls).toContain('drivers_rescan');
  });

  it('selects a table for the Table tab on click', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <ConnectionTree />
      </TooltipProvider>,
    );
    await user.click(screen.getByText('local (SQLite)'));
    // SQLite: database "main" (current) -> schema "main" -> tables.
    await user.click(await screen.findByText('main'));
    await waitFor(() => expect(screen.getAllByText('main')).toHaveLength(2));
    await user.click(screen.getAllByText('main')[1]);
    await user.click(await screen.findByText('measurements'));
    const sel = useUiStore.getState().selectedTable;
    expect(sel?.connId).toBe('c-local');
    expect(sel?.ref.name).toBe('measurements');
    expect(useUiStore.getState().centerTab).toBe('table');
  });

  it('refreshes a connection by dropping the backend metadata cache before reloading', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <ConnectionTree />
      </TooltipProvider>,
    );
    await user.click(screen.getByText('prod (PostgreSQL)'));
    await user.click(await screen.findByText('orders'));
    await screen.findByText('public');
    mockBackend.calls.length = 0;

    await user.click(screen.getByRole('button', { name: t('tree.refresh') }));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_databases'));
    // The invalidation comes first, so the subtrees that reload afterwards (the expanded
    // database's schemas here) come from the server, not from the backend cache.
    expect(mockBackend.calls[0]).toBe('catalog_invalidate');
    expect(mockBackend.calls.indexOf('catalog_invalidate')).toBeLessThan(mockBackend.calls.indexOf('catalog_databases'));
    await waitFor(() => expect(mockBackend.calls).toContain('catalog_schemas'));
    expect(await screen.findByText('public')).toBeInTheDocument();
  });
});
