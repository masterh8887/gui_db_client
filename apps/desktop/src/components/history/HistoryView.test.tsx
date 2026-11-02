import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { HistoryEntry } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { historyList } from '@/lib/commands';
import { stateLabel } from '@/lib/i18n';
import { mockConnections, mockDrivers, mockHistory } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useTabsStore } from '@/stores/tabs';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { __resetHistoryViewForTests, HistoryView } from './HistoryView';
import { hs } from './strings';

vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, historyList: vi.fn(actual.historyList) };
});

const prodEntries = () => mockHistory().filter((h) => h.connId === 'c-prod');
const otherEntry = () => mockHistory().find((h) => h.connId !== 'c-prod') as HistoryEntry;

function renderView(tabId = 'h1') {
  return render(
    <TooltipProvider>
      <HistoryView tabId={tabId} connId="c-prod" />
    </TooltipProvider>,
  );
}

function spyOpen() {
  const fn = vi.fn(async () => 'q1');
  useWorkspaceStore.setState({ openFromHistory: fn });
  return fn;
}

const grid = () => screen.getByRole('grid', { name: hs('gridLabel') });
const bodyRows = () => within(grid()).getAllByRole('row').slice(1);
const rowOf = (label: string) => screen.getByText(label).closest('tr') as HTMLElement;

async function chooseConnection(user: ReturnType<typeof userEvent.setup>, label: string) {
  await user.click(screen.getByRole('combobox', { name: hs('connection') }));
  await user.click(await screen.findByRole('option', { name: label }));
}

describe('HistoryView', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    __resetHistoryViewForTests();
    vi.mocked(historyList).mockClear();
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('loads the workspace connection’s history into a dense grid', async () => {
    renderView();
    const first = prodEntries()[0];
    expect(await screen.findByText(first.label)).toBeInTheDocument();
    expect(vi.mocked(historyList)).toHaveBeenCalledWith('c-prod', 500);
    expect(bodyRows()).toHaveLength(prodEntries().length);
    // State chip with its label, rows in full, the error of a failed export.
    expect(within(rowOf(first.label)).getByText(stateLabel('done'))).toBeInTheDocument();
    expect(within(rowOf(first.label)).getByText(Number(first.rows).toLocaleString('en-US'))).toBeInTheDocument();
    expect(screen.getByText(/No space left on device/)).toBeInTheDocument();
  });

  it('reloads through history_list when the connection filter changes', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByText(prodEntries()[0].label);
    await chooseConnection(user, hs('allConnections'));
    expect(vi.mocked(historyList)).toHaveBeenLastCalledWith(undefined, 500);
    expect(await screen.findByText(otherEntry().label)).toBeInTheDocument();
    const local = mockConnections().find((c) => c.id === otherEntry().connId);
    await chooseConnection(user, local?.name ?? '');
    expect(vi.mocked(historyList)).toHaveBeenLastCalledWith(otherEntry().connId, 500);
    await waitFor(() => expect(screen.queryByText(prodEntries()[0].label)).not.toBeInTheDocument());
  });

  it('keeps the filter and search of the tab when it is shown again', async () => {
    const user = userEvent.setup();
    const view = renderView('h7');
    await screen.findByText(prodEntries()[0].label);
    await user.type(screen.getByRole('searchbox'), 'customers');
    view.unmount();
    renderView('h7');
    expect(screen.getByRole('searchbox')).toHaveValue('customers');
    expect(await screen.findByText('customers_kr')).toBeInTheDocument();
    expect(screen.queryByText(prodEntries()[0].label)).not.toBeInTheDocument();
  });

  it('opens a clicked row in a new query tab, once per double-click', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderView();
    const first = prodEntries()[0];
    await user.click(await screen.findByText(first.label));
    expect(open).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ id: first.id }));
    await user.dblClick(screen.getByText(first.label));
    expect(open).toHaveBeenCalledTimes(2);
  });

  it('runs an entry again from the first column without also opening it', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderView();
    const first = prodEntries()[0];
    await screen.findByText(first.label);
    await user.click(within(rowOf(first.label)).getByRole('button', { name: hs('runAgain') }));
    expect(open).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ id: first.id }), { run: true });
  });

  it('is keyboard driven: arrows select, Enter opens, Mod+Enter runs again, Mod+C copies the SQL', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderView();
    const entries = prodEntries();
    await screen.findByText(entries[0].label);
    grid().focus();
    await user.keyboard('{ArrowDown}{ArrowDown}');
    expect(bodyRows()[1]).toHaveAttribute('aria-selected', 'true');
    expect(open).not.toHaveBeenCalled();
    await user.keyboard('{Enter}');
    expect(open).toHaveBeenLastCalledWith(expect.objectContaining({ id: entries[1].id }));
    await user.keyboard('{Control>}{Enter}{/Control}');
    expect(open).toHaveBeenLastCalledWith(expect.objectContaining({ id: entries[1].id }), { run: true });
    await user.keyboard('{Control>}c{/Control}');
    expect(await navigator.clipboard.readText()).toBe(entries[1].sql);
  });

  it('switches workspaces for another connection’s row', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByText(prodEntries()[0].label);
    await chooseConnection(user, hs('allConnections'));
    const o = otherEntry();
    await user.click(await screen.findByText(o.label));
    await waitFor(() => expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: o.connId }));
    await waitFor(() => expect(useTabsStore.getState().sets[o.connId]?.ids.length).toBe(1));
    const id = useTabsStore.getState().sets[o.connId].ids[0];
    expect(useExtractStore.getState().docs[id]).toMatchObject({ sql: o.sql, name: o.label });
  });

  it('opens the output folder of an entry from its context menu', async () => {
    const user = userEvent.setup();
    renderView();
    const first = prodEntries()[0];
    await screen.findByText(first.label);
    fireEvent.contextMenu(rowOf(first.label));
    await user.click(await screen.findByRole('menuitem', { name: hs('openFolder') }));
    await waitFor(() => expect(mockBackend.calls).toContain('open_path'));
  });

  it('refreshes the current page', async () => {
    const user = userEvent.setup();
    renderView();
    await screen.findByText(prodEntries()[0].label);
    await waitFor(() => expect(screen.getByRole('button', { name: hs('refresh') })).toBeEnabled());
    await user.click(screen.getByRole('button', { name: hs('refresh') }));
    expect(vi.mocked(historyList)).toHaveBeenCalledTimes(2);
    expect(vi.mocked(historyList)).toHaveBeenLastCalledWith('c-prod', 500);
  });
});
