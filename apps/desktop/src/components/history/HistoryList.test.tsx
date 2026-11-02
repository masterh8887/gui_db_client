import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { HistoryEntry } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { historyList } from '@/lib/commands';
import { mockConnections, mockDrivers, mockHistory } from '@/lib/mock-data';
import { stateLabel } from '@/lib/i18n';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useTabsStore } from '@/stores/tabs';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { __resetHistoryListForTests, HistoryList } from './HistoryList';
import { hs } from './strings';

vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, historyList: vi.fn(actual.historyList) };
});

const prodEntries = () => mockHistory().filter((h) => h.connId === 'c-prod');
const otherEntry = () => mockHistory().find((h) => h.connId !== 'c-prod') as HistoryEntry;

function renderList(connId = 'c-prod') {
  return render(
    <TooltipProvider>
      <HistoryList connId={connId} />
    </TooltipProvider>,
  );
}

/** Replace `openFromHistory` with a spy (the real one opens workspaces). */
function spyOpen() {
  const fn = vi.fn(async () => 'q1');
  useWorkspaceStore.setState({ openFromHistory: fn });
  return fn;
}

const list = () => screen.getByRole('listbox', { name: hs('listLabel') });

describe('HistoryList', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    __resetHistoryListForTests();
    vi.mocked(historyList).mockClear();
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('loads this connection’s history from the backend and shows two-line rows', async () => {
    renderList();
    const first = prodEntries()[0];
    expect(await screen.findByText(first.label)).toBeInTheDocument();
    expect(vi.mocked(historyList)).toHaveBeenCalledWith('c-prod', 500);
    expect(within(list()).getAllByRole('option')).toHaveLength(prodEntries().length);
    // The state is an icon with a name, never colour alone.
    expect(within(list()).getAllByRole('img', { name: stateLabel('done') }).length).toBeGreaterThan(0);
    expect(screen.getByText(hs('count', { n: prodEntries().length }))).toBeInTheDocument();
  });

  it('switches to every connection, naming the other connections on their rows', async () => {
    const user = userEvent.setup();
    renderList();
    await screen.findByText(prodEntries()[0].label);
    await user.click(screen.getByRole('radio', { name: hs('scopeAll') }));
    expect(vi.mocked(historyList)).toHaveBeenLastCalledWith(undefined, 500);
    const o = otherEntry();
    const row = (await screen.findByText(o.label)).closest('[role="option"]') as HTMLElement;
    const name = mockConnections().find((c) => c.id === o.connId)?.name ?? '';
    expect(row).toHaveTextContent(name);
    // This connection's rows do not repeat its name.
    const own = screen.getByText(prodEntries()[0].label).closest('[role="option"]') as HTMLElement;
    expect(own).not.toHaveTextContent(mockConnections().find((c) => c.id === 'c-prod')?.name ?? '?');
  });

  it('remembers the scope for the session when the sidebar switches back', async () => {
    const user = userEvent.setup();
    const view = renderList();
    await screen.findByText(prodEntries()[0].label);
    await user.click(screen.getByRole('radio', { name: hs('scopeAll') }));
    view.unmount();
    renderList();
    expect(screen.getByRole('radio', { name: hs('scopeAll') })).toHaveAttribute('aria-checked', 'true');
    expect(await screen.findByText(otherEntry().label)).toBeInTheDocument();
  });

  it('searches the loaded page by name, SQL and file', async () => {
    const user = userEvent.setup();
    renderList();
    await screen.findByText(prodEntries()[0].label);
    await user.type(screen.getByRole('searchbox'), 'country');
    expect(within(list()).getAllByRole('option')).toHaveLength(1);
    expect(screen.getByText('customers_kr')).toBeInTheDocument();
    await user.clear(screen.getByRole('searchbox'));
    await user.type(screen.getByRole('searchbox'), '.parquet');
    expect(within(list()).getAllByRole('option')).toHaveLength(1);
    await user.type(screen.getByRole('searchbox'), 'zzz');
    expect(screen.getByText(hs('noMatch'))).toBeInTheDocument();
  });

  it('opens a clicked entry in a new query tab, once per double-click', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderList();
    const first = prodEntries()[0];
    await user.click(await screen.findByText(first.label));
    expect(open).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ id: first.id }));
    await user.dblClick(screen.getByText(first.label));
    expect(open).toHaveBeenCalledTimes(2);
  });

  it('runs an entry again from its hover button', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderList();
    const first = prodEntries()[0];
    const row = (await screen.findByText(first.label)).closest('[role="option"]') as HTMLElement;
    await user.click(within(row).getByRole('button', { name: hs('runAgain') }));
    expect(open).toHaveBeenCalledTimes(1);
    expect(open).toHaveBeenCalledWith(expect.objectContaining({ id: first.id }), { run: true });
  });

  it('enters the list from the search with ↓, moves with the arrows, Enter opens, Mod+Enter runs again', async () => {
    const user = userEvent.setup();
    const open = spyOpen();
    renderList();
    const entries = prodEntries();
    await screen.findByText(entries[0].label);
    await user.click(screen.getByRole('searchbox'));
    await user.keyboard('{ArrowDown}');
    expect(list()).toHaveFocus();
    const options = within(list()).getAllByRole('option');
    expect(options[0]).toHaveAttribute('aria-selected', 'true');
    await user.keyboard('{ArrowDown}');
    expect(options[1]).toHaveAttribute('aria-selected', 'true');
    expect(list()).toHaveAttribute('aria-activedescendant', options[1].id);
    await user.keyboard('{Enter}');
    expect(open).toHaveBeenLastCalledWith(expect.objectContaining({ id: entries[1].id }));
    await user.keyboard('{Control>}{Enter}{/Control}');
    expect(open).toHaveBeenLastCalledWith(expect.objectContaining({ id: entries[1].id }), { run: true });
    await user.keyboard('{Control>}c{/Control}');
    expect(await navigator.clipboard.readText()).toBe(entries[1].sql);
  });

  it('switches workspaces for another connection’s entry and opens it with its SQL', async () => {
    const user = userEvent.setup();
    renderList();
    await screen.findByText(prodEntries()[0].label);
    await user.click(screen.getByRole('radio', { name: hs('scopeAll') }));
    const o = otherEntry();
    await user.click(await screen.findByText(o.label));
    await waitFor(() => expect(useWorkspaceStore.getState()).toMatchObject({ mode: 'workspace', openConnId: o.connId }));
    await waitFor(() => expect(useTabsStore.getState().sets[o.connId]?.ids.length).toBe(1));
    const id = useTabsStore.getState().sets[o.connId].ids[0];
    expect(useExtractStore.getState().docs[id]).toMatchObject({ sql: o.sql, name: o.label });
  });

  it('opens the full history tab from the footer', async () => {
    const user = userEvent.setup();
    renderList();
    await screen.findByText(prodEntries()[0].label);
    await user.click(screen.getByRole('button', { name: hs('fullHistory') }));
    expect(Object.values(useTabsStore.getState().tabs)).toEqual([expect.objectContaining({ kind: 'history', connId: 'c-prod' })]);
  });

  it('shows the empty state for a connection without history', async () => {
    renderList('c-staging');
    expect(await screen.findByText(hs('empty'))).toBeInTheDocument();
    expect(vi.mocked(historyList)).toHaveBeenCalledWith('c-staging', 500);
  });

  it('shows a load error with a retry', async () => {
    const user = userEvent.setup();
    vi.mocked(historyList).mockRejectedValueOnce(new Error('history db locked'));
    renderList();
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('history db locked');
    await user.click(within(alert).getByRole('button', { name: hs('retry') }));
    expect(await screen.findByText(prodEntries()[0].label)).toBeInTheDocument();
    await act(async () => {});
    expect(vi.mocked(historyList)).toHaveBeenCalledTimes(2);
  });
});
