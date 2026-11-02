import type { Connection, ImportedConnection } from 'ipc-types';
import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore, type ImportSaveResult } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { ImportDbeaverFlow } from './ImportDbeaverDialog';
import { is } from './ImportDbeaverDialog.strings';

const FILE = 'C:\\Users\\me\\AppData\\Roaming\\DBeaverData\\workspace6\\General\\.dbeaver\\data-sources.json';

function startFlow(path: string | null) {
  vi.spyOn(window, 'prompt').mockReturnValue(path);
  render(
    <TooltipProvider>
      <ImportDbeaverFlow />
    </TooltipProvider>,
  );
  act(() => useUiStore.getState().setImportDbeaverOpen(true));
}

const checkbox = (name: string) => screen.getByRole('checkbox', { name });
const rowOf = (name: string) => checkbox(name).closest('tr')!;

describe('ImportDbeaverFlow', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });
  afterEach(() => vi.restoreAllMocks());

  it('picks the file, shows its path and the rows, and imports the selected ones without passwords', async () => {
    const user = userEvent.setup();
    startFlow(FILE);
    const dialog = await screen.findByRole('dialog', { name: is('title') });
    await within(dialog).findByRole('checkbox', { name: 'erp-oracle (DBeaver)' });
    expect(within(dialog).getByTitle(FILE)).toBeInTheDocument();
    expect(within(dialog).getByText(is('passwordNote'))).toBeInTheDocument();

    // Default selection: ok and driverMissing rows; unsupported rows cannot be picked.
    expect(checkbox('erp-oracle (DBeaver)')).toBeChecked();
    expect(within(rowOf('erp-oracle (DBeaver)')).getByText(is('statusDriverMissing'))).toBeInTheDocument();
    expect(checkbox('staging-mysql (DBeaver)')).toBeChecked();
    expect(within(rowOf('staging-mysql (DBeaver)')).getByText(is('statusOk'))).toBeInTheDocument();
    expect(checkbox('events-clickhouse (DBeaver)')).not.toBeChecked();
    expect(checkbox('events-clickhouse (DBeaver)')).toBeDisabled();
    expect(within(rowOf('events-clickhouse (DBeaver)')).getByText(is('statusUnsupported'))).toBeInTheDocument();

    // Deselect all, then select all again (unsupported stays off).
    await user.click(checkbox(is('selectAll')));
    expect(screen.getByRole('button', { name: is('import', { count: 0 }) })).toBeDisabled();
    await user.click(checkbox(is('selectAll')));
    expect(checkbox('events-clickhouse (DBeaver)')).not.toBeChecked();

    await user.click(screen.getByRole('button', { name: is('import', { count: 2 }) }));
    await waitFor(() => expect(useUiStore.getState().importDbeaverOpen).toBe(false));
    const names = useConnectionsStore.getState().connections.map((c) => c.name);
    expect(names).toEqual(expect.arrayContaining(['erp-oracle (DBeaver)', 'staging-mysql (DBeaver)']));
    expect(names).not.toContain('events-clickhouse (DBeaver)');
    const imported = useConnectionsStore.getState().connections.find((c) => c.name === 'staging-mysql (DBeaver)');
    expect(imported?.hasPassword).toBe(false);
    expect(useToastStore.getState().toasts.at(-1)?.message).toBe(is('done', { count: 2 }));
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('shows a parse failure as a danger notice', async () => {
    startFlow('C:\\missing\\data-sources.json');
    expect(await screen.findByText(is('parseFailed'))).toBeInTheDocument();
    expect(screen.getByText(/no such file/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /가져오기$/ })).toBeNull();
    await userEvent.setup().click(screen.getByRole('button', { name: is('close') }));
    expect(useUiStore.getState().importDbeaverOpen).toBe(false);
  });

  it('closes when the file picker is cancelled', async () => {
    startFlow(null);
    await waitFor(() => expect(useUiStore.getState().importDbeaverOpen).toBe(false));
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('after a partial failure, saved rows are locked and a retry imports only the failures', async () => {
    const user = userEvent.setup();
    const calls: string[][] = [];
    const fake = vi.fn(async (rows: ImportedConnection[]): Promise<ImportSaveResult> => {
      calls.push(rows.map((r) => r.connection.name));
      const asConn = (r: ImportedConnection): Connection => ({ ...r.connection, id: `new-${r.connection.id}` });
      if (calls.length === 1) {
        return { saved: [asConn(rows[0])], failed: [{ index: 1, name: rows[1].connection.name, error: 'boom: duplicate key' }] };
      }
      return { saved: rows.map(asConn), failed: [] };
    });
    useConnectionsStore.setState({ saveImported: fake });
    startFlow(FILE);
    await screen.findByRole('checkbox', { name: 'erp-oracle (DBeaver)' });

    await user.click(screen.getByRole('button', { name: is('import', { count: 2 }) }));
    expect(await screen.findByText(is('failed', { count: 1 }))).toBeInTheDocument();
    expect(screen.getByText(/boom: duplicate key/)).toBeInTheDocument();
    expect(checkbox('erp-oracle (DBeaver)')).toBeChecked();
    expect(checkbox('erp-oracle (DBeaver)')).toBeDisabled();
    expect(within(rowOf('erp-oracle (DBeaver)')).getByText(is('statusDone'))).toBeInTheDocument();
    expect(useUiStore.getState().importDbeaverOpen).toBe(true);

    await user.click(screen.getByRole('button', { name: is('import', { count: 1 }) }));
    await waitFor(() => expect(useUiStore.getState().importDbeaverOpen).toBe(false));
    expect(calls).toEqual([['erp-oracle (DBeaver)', 'staging-mysql (DBeaver)'], ['staging-mysql (DBeaver)']]);
  });
});
