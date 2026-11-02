import type { Connection } from 'ipc-types';
import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { connSave } from '@/lib/commands';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { DbPasswordDialog } from './DbPasswordDialog';
import { dp } from './DbPasswordDialog.strings';

/** A PostgreSQL connection saved with "ask every time" (no password in the keyring). */
async function askEveryTimeConnection(name = 'ask (PG)'): Promise<Connection> {
  const saved = await connSave({
    id: '',
    name,
    tag: 'production',
    driver: 'postgres',
    host: 'db-replica.internal',
    port: 5432,
    database: 'orders',
    user: 'analyst',
    fields: {},
    session: {},
    savePassword: false,
    hasPassword: false,
    hasSshSecret: false,
  });
  const others = useConnectionsStore.getState().connections;
  useConnectionsStore.setState({ connections: [...others, saved], drivers: mockDrivers(), loaded: true });
  mockBackend.calls.length = 0;
  return saved;
}

/** Spy on the store actions the resume step calls (DESIGN.md §9: no other owner's component). */
function spyResume() {
  const openWorkspace = vi.fn(async () => true);
  const ensureScope = vi.fn(async () => undefined);
  const loadDatabases = vi.fn(async () => []);
  const run = vi.fn(async () => undefined);
  const explain = vi.fn(async () => undefined);
  useWorkspaceStore.setState({ openWorkspace, ensureScope });
  useCatalogStore.setState({ loadDatabases });
  useExtractStore.setState({ run, explain } as never);
  return { openWorkspace, ensureScope, loadDatabases, run, explain };
}

function renderDialog() {
  return render(
    <TooltipProvider>
      <DbPasswordDialog />
    </TooltipProvider>,
  );
}

const field = () => screen.getByLabelText(dp('label', { user: 'analyst', name: 'ask (PG)' }));

describe('DbPasswordDialog ("ask every time" connections)', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.configure({ latencyMs: 0 });
  });

  it('shows the head prompt with the connection tag, sends the password once through conn_test, then opens the workspace', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    const then = vi.fn();
    renderDialog();
    act(() => useUiStore.getState().askDbPassword(conn.id, 'open', { then }));

    const sheet = await screen.findByRole('dialog', { name: dp('title') });
    expect(sheet).toHaveTextContent('production');
    await user.type(field(), 's3cret');
    await user.click(screen.getByRole('button', { name: dp('submit') }));

    // The `then` follow-up opens its own tab, so the workspace opens without the empty "쿼리 1".
    await waitFor(() => expect(spies.openWorkspace).toHaveBeenCalledWith(conn.id, { skipEmptyTab: true }));
    expect(spies.openWorkspace).toHaveBeenCalledTimes(1);
    expect(then).toHaveBeenCalledTimes(1);
    expect(mockBackend.calls.filter((c) => c === 'conn_test')).toHaveLength(1);
    expect(useConnectionsStore.getState().unlocked[conn.id]).toBe(true);
    expect(useUiStore.getState().dbPasswordPrompts).toHaveLength(0);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });

  it('a plain open (no follow-up) opens the workspace with its default empty query tab', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    renderDialog();
    act(() => useUiStore.getState().askDbPassword(conn.id, 'open'));
    await user.type(field(), 's3cret{Enter}');

    await waitFor(() => expect(spies.openWorkspace).toHaveBeenCalledTimes(1));
    expect(spies.openWorkspace).toHaveBeenCalledWith(conn.id);
  });

  it('resumes browse and run requests merged into one prompt, in order', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    renderDialog();
    act(() => {
      useUiStore.getState().askDbPassword(conn.id, 'browse');
      useUiStore.getState().askDbPassword(conn.id, 'run', { tabId: 'q7' });
      useUiStore.getState().askDbPassword(conn.id, 'explain', { tabId: 'q8' });
    });
    expect(useUiStore.getState().dbPasswordPrompts).toHaveLength(1);
    expect(screen.getAllByRole('dialog')).toHaveLength(1);

    await user.type(field(), 's3cret{Enter}');
    await waitFor(() => expect(spies.explain).toHaveBeenCalledWith('q8'));
    expect(spies.loadDatabases).toHaveBeenCalledWith(conn.id, true);
    expect(spies.ensureScope).toHaveBeenCalledWith(conn.id);
    expect(spies.run).toHaveBeenCalledWith('q7');
    expect(spies.loadDatabases.mock.invocationCallOrder[0]).toBeLessThan(spies.run.mock.invocationCallOrder[0]);
    expect(spies.run.mock.invocationCallOrder[0]).toBeLessThan(spies.explain.mock.invocationCallOrder[0]);
    expect(mockBackend.calls.indexOf('conn_test')).toBeGreaterThanOrEqual(0);
  });

  it('picks up a request merged while the password test was in flight', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    mockBackend.configure({ latencyMs: 30 });
    renderDialog();
    act(() => useUiStore.getState().askDbPassword(conn.id, 'test'));
    await user.type(field(), 's3cret');
    await user.click(screen.getByRole('button', { name: dp('submit') }));
    act(() => useUiStore.getState().askDbPassword(conn.id, 'run', { tabId: 'q1' }));
    await waitFor(() => expect(spies.run).toHaveBeenCalledWith('q1'));
  });

  it('keeps the sheet open with the error when the password is wrong, and forgets the typed secret', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    renderDialog();
    act(() => useUiStore.getState().askDbPassword(conn.id, 'open'));
    await user.type(field(), 'wrong');
    await user.click(screen.getByRole('button', { name: dp('submit') }));

    expect(await screen.findByText(/password authentication failed/)).toBeInTheDocument();
    expect(useUiStore.getState().dbPasswordPrompts[0]?.connId).toBe(conn.id);
    expect(useConnectionsStore.getState().unlocked[conn.id]).toBeUndefined();
    expect(field()).toHaveValue('');
    expect(spies.openWorkspace).not.toHaveBeenCalled();
  });

  it('Esc is the cancel answer: the prompt goes with its requests and nothing resumes', async () => {
    const user = userEvent.setup();
    const conn = await askEveryTimeConnection();
    const spies = spyResume();
    renderDialog();
    act(() => {
      useUiStore.getState().askDbPassword(conn.id, 'browse');
      useUiStore.getState().askDbPassword(conn.id, 'run', { tabId: 'q1' });
    });
    await screen.findByRole('dialog', { name: dp('title') });
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().dbPasswordPrompts).toHaveLength(0);
    expect(mockBackend.calls).not.toContain('conn_test');
    expect(spies.loadDatabases).not.toHaveBeenCalled();
    expect(spies.run).not.toHaveBeenCalled();
  });

  it('shows queued prompts of other connections one after the other, each with an empty field', async () => {
    const user = userEvent.setup();
    useConnectionsStore.setState({ connections: mockConnections() });
    const first = await askEveryTimeConnection();
    const second = await askEveryTimeConnection('ask two');
    spyResume();
    renderDialog();
    act(() => {
      useUiStore.getState().askDbPassword(first.id, 'test');
      useUiStore.getState().askDbPassword(second.id, 'test');
    });
    await user.type(field(), 'typed');
    await user.click(screen.getByRole('button', { name: dp('cancel') }));
    const next = await screen.findByLabelText(dp('label', { user: 'analyst', name: 'ask two' }));
    expect(next).toHaveValue('');
  });
});
