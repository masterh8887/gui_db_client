// Welcome as the connection manager of several windows (docs/db-copy.md 1.1): "창 N" chips,
// the "열린 연결" group, Mod+Enter / 새 창에서 열기, the copy entry points, the write-allowed
// mark and the other-instances chip.

import type { Connection, WindowInfo } from 'ipc-types';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useToastStore } from '@/lib/toast';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useUiStore } from '@/stores/ui';
import { useWindowsStore } from '@/stores/windows';
import { resetStores } from '@/test/stores';
import { connectionMenuItems } from './ConnectionRow';
import { ws } from './strings';
import { WelcomeScreen } from './WelcomeScreen';

const win = (label: string, index: number, connId?: string): WindowInfo => ({
  label,
  index,
  focused: false,
  title: 'BulkDB',
  ...(connId ? { connId, connName: connId } : {}),
});

async function renderWelcome() {
  render(
    <TooltipProvider>
      <WelcomeScreen />
    </TooltipProvider>,
  );
  await screen.findByText('v0.1.0-mock');
}

const list = () => screen.getByRole('listbox', { name: ws('listLabel') });
const row = (id: string) => list().querySelector<HTMLElement>(`[data-conn-id="${id}"]`)!;

describe('Welcome with several windows', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true, selectedId: 'c-prod' });
    useWindowsStore.setState({ self: { label: 'main', index: 1 }, windows: [win('main', 1), win('w-2', 2, 'c-staging'), win('w-3', 3, 'c-staging')] });
  });

  it('shows a chip per other window and focuses it on click', async () => {
    await renderWelcome();
    const chips = within(row('c-staging')).getAllByRole('button', { name: /창 \d/ });
    expect(chips.map((c) => c.textContent)).toEqual([ws('windowChip', { index: 2 }), ws('windowChip', { index: 3 })]);
    expect(within(row('c-prod')).queryByRole('button', { name: /창 \d/ })).toBeNull();
    const focus = vi.fn(async () => undefined);
    useWindowsStore.setState({ focus });
    await userEvent.click(chips[1]!);
    expect(focus).toHaveBeenCalledWith('w-3');
    // The click does not select or open the row.
    expect(useConnectionsStore.getState().selectedId).toBe('c-prod');
  });

  it('lists connections open in any window first under 열린 연결', async () => {
    await renderWelcome();
    const groups = within(list()).getAllByRole('group');
    expect(groups.map((g) => g.getAttribute('aria-label'))).toEqual([ws('groupOpen'), ws('groupOthers')]);
    expect(within(groups[0]!).getAllByRole('option').map((o) => o.getAttribute('data-conn-id'))).toEqual(['c-staging']);
    // Not while searching, not when grouped by tag.
    act(() => useUiStore.getState().setWelcomeGroupBy('tag'));
    expect(within(list()).getAllByRole('group').map((g) => g.getAttribute('aria-label'))).not.toContain(ws('groupOpen'));
    act(() => useUiStore.getState().setWelcomeGroupBy('none'));
    act(() => useWindowsStore.setState({ windows: [win('main', 1)] }));
    expect(within(list()).queryAllByRole('group')).toHaveLength(0);
  });

  it('Mod+Enter opens the selected connection in a new window', async () => {
    const openNew = vi.fn(async () => undefined);
    useWindowsStore.setState({ openNew });
    await renderWelcome();
    fireEvent.keyDown(list(), { key: 'Enter', code: 'Enter', ctrlKey: true, metaKey: true });
    expect(openNew).toHaveBeenCalledWith('c-prod');
    // From the search field too; plain Enter still opens here.
    const search = screen.getByRole('searchbox', { name: ws('searchLabel') });
    fireEvent.keyDown(search, { key: 'Enter', code: 'Enter', ctrlKey: true, metaKey: true });
    expect(openNew).toHaveBeenCalledTimes(2);
  });

  it('row menu: new window, copy as source / target, clear write access', async () => {
    const open = vi.fn();
    useCopyStore.setState({ open });
    const openNew = vi.fn(async () => undefined);
    useWindowsStore.setState({ openNew });
    const prod = mockConnections().find((c) => c.id === 'c-prod')!;
    const items = connectionMenuItems(prod, { isOpen: false, mac: false, onDelete: () => undefined });
    const byId = (id: string) => items.find((i) => i.id === id);
    expect(items.map((i) => i.id).slice(0, 7)).toEqual(['connect', 'openInNewWindow', 'test', 'edit', 'duplicate', 'copyAsSource', 'copyAsTarget']);
    expect(byId('clearWriteAllowed')).toBeUndefined();
    byId('openInNewWindow')?.onSelect?.();
    expect(openNew).toHaveBeenCalledWith('c-prod');
    byId('copyAsSource')?.onSelect?.();
    expect(open).toHaveBeenLastCalledWith({ source: { connId: 'c-prod', database: prod.database } });
    byId('copyAsTarget')?.onSelect?.();
    expect(open).toHaveBeenLastCalledWith({ target: { connId: 'c-prod' } });
    const writable: Connection = { ...prod, writeAllowed: true };
    expect(connectionMenuItems(writable, { isOpen: false, mac: false, onDelete: () => undefined }).map((i) => i.id)).toContain('clearWriteAllowed');
  });

  it('offers the copy entry points only for copy-supported drivers', () => {
    const prod = mockConnections().find((c) => c.id === 'c-prod')!; // postgres: supported
    const pgIds = connectionMenuItems(prod, { isOpen: false, mac: false, onDelete: () => undefined }).map((i) => i.id);
    expect(pgIds).toEqual(expect.arrayContaining(['copyAsSource', 'copyAsTarget']));
    // mssql/oracle/odbc cannot be copied; the items must not appear (the wizard would gate on them).
    const mssql: Connection = { ...prod, driver: 'mssql' };
    const ids = connectionMenuItems(mssql, { isOpen: false, mac: false, onDelete: () => undefined }).map((i) => i.id);
    expect(ids).not.toContain('copyAsSource');
    expect(ids).not.toContain('copyAsTarget');
  });

  it('toasts when clearing write access fails (the row menu has no dialog for it)', async () => {
    const writable: Connection = { ...mockConnections().find((c) => c.id === 'c-prod')!, writeAllowed: true };
    useConnectionsStore.setState({
      save: async () => {
        throw new Error('keyring unavailable');
      },
    });
    const before = useToastStore.getState().toasts.length;
    const clear = connectionMenuItems(writable, { isOpen: false, mac: false, onDelete: () => undefined }).find((i) => i.id === 'clearWriteAllowed')!;
    clear.onSelect?.();
    await waitFor(() => {
      const added = useToastStore.getState().toasts.slice(before);
      expect(added.some((t) => t.tone === 'danger' && t.message.includes('keyring unavailable'))).toBe(true);
    });
  });

  it('marks write-allowed connections and offers 데이터베이스 복사… in the left pane', async () => {
    const open = vi.fn();
    useCopyStore.setState({ open });
    useConnectionsStore.setState({ connections: mockConnections().map((c) => (c.id === 'c-staging' ? { ...c, writeAllowed: true } : c)) });
    await renderWelcome();
    const mark = row('c-staging').querySelector('[data-write-allowed]');
    expect(mark).not.toBeNull();
    // The mark is announced to screen readers (role + aria-label), not mouse-tooltip only.
    expect(mark?.getAttribute('role')).toBe('img');
    expect(mark?.getAttribute('aria-label')).toBe(ws('writeAllowedTip'));
    expect(row('c-prod').querySelector('[data-write-allowed]')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: ws('copyDatabase') }));
    expect(open).toHaveBeenCalledWith();
  });

  it('says when other BulkDB processes run', async () => {
    await renderWelcome();
    expect(document.querySelector('[data-instances-chip]')).toBeNull();
    act(() =>
      useWindowsStore.setState({
        instances: { selfPid: 1, others: [{ pid: 2, instanceId: 'x', kind: 'desktop', startedAt: '2026-10-27T10:00:00Z', version: '0.1.0' }] },
      }),
    );
    await waitFor(() => expect(document.querySelector('[data-instances-chip]')).toHaveTextContent(ws('otherInstances', { n: 1 })));
  });
});
