import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useLogsStore } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { AppShell } from './AppShell';

// Every slot owned by another team is replaced by a landmark: this test covers the frame only.
const { slot } = vi.hoisted(() => ({
  slot: (name: string) =>
    function Slot() {
      return <div data-testid={`slot-${name}`} />;
    },
}));
vi.mock('@/components/AppEvents', () => ({ AppEvents: slot('AppEvents') }));
vi.mock('@/components/welcome/WelcomeScreen', () => ({
  WelcomeScreen: ({ headerEnd }: { headerEnd?: React.ReactNode }) => (
    <div data-testid="slot-WelcomeScreen">
      {headerEnd}
    </div>
  ),
}));
vi.mock('@/components/sidebar/Sidebar', () => ({ Sidebar: slot('Sidebar') }));
vi.mock('@/components/history/HistoryList', () => ({ HistoryList: slot('HistoryList') }));
vi.mock('@/components/history/HistoryView', () => ({ HistoryView: slot('HistoryView') }));
vi.mock('@/components/query/QueryTabView', () => ({ QueryTabView: ({ tabId }: { tabId: string }) => <div data-testid={`slot-QueryTabView-${tabId}`} /> }));
vi.mock('@/components/query/PreflightSheet', () => ({ PreflightSheet: slot('PreflightSheet') }));
vi.mock('@/components/table/StructureTabView', () => ({ StructureTabView: slot('StructureTabView') }));
vi.mock('@/components/table/TableInspector', () => ({ TableInspector: slot('TableInspector') }));
vi.mock('@/components/monitor/ActivityPanel', () => ({ ActivityPanel: slot('ActivityPanel') }));
vi.mock('@/components/monitor/JobChip', () => ({ JobChip: ({ variant }: { variant: string }) => <div data-testid={`slot-JobChip-${variant}`} /> }));
vi.mock('@/components/log/ConsoleDrawer', () => ({
  ConsoleDrawer: ({ onClose }: { onClose: () => void }) => (
    <div data-testid="slot-ConsoleDrawer">
      <button type="button" onClick={onClose}>
        close console
      </button>
    </div>
  ),
}));
vi.mock('@/components/log/LogStatusButton', () => ({ LogStatusButton: slot('LogStatusButton') }));
vi.mock('@/components/palette/CommandPalette', () => ({ CommandPalette: slot('CommandPalette') }));
vi.mock('@/components/dialogs/ConnectionDialog', () => ({ ConnectionDialog: slot('ConnectionDialog') }));
vi.mock('@/components/dialogs/ImportDbeaverDialog', () => ({ ImportDbeaverFlow: slot('ImportDbeaverFlow') }));
vi.mock('@/components/dialogs/SettingsDialog', () => ({ SettingsDialog: slot('SettingsDialog') }));
vi.mock('@/components/dialogs/AboutDialog', () => ({ AboutDialog: slot('AboutDialog') }));
vi.mock('@/components/dialogs/HostKeyDialog', () => ({ HostKeyDialog: slot('HostKeyDialog') }));
vi.mock('@/components/dialogs/SshPasswordDialog', () => ({ SshPasswordDialog: slot('SshPasswordDialog') }));
vi.mock('@/components/dialogs/DbPasswordDialog', () => ({ DbPasswordDialog: slot('DbPasswordDialog') }));

describe('AppShell', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });

  it('starts on Welcome, switches to the workspace and back; mounts every slot once', async () => {
    render(<AppShell />);
    expect(document.documentElement.dataset.platform).toMatch(/^(mac|windows)$/);
    expect(screen.getByTestId('slot-WelcomeScreen')).toBeInTheDocument();
    expect(screen.getByTestId('slot-JobChip-welcome')).toBeInTheDocument();
    for (const name of [
      'AppEvents', 'CommandPalette', 'ConnectionDialog', 'ImportDbeaverFlow', 'SettingsDialog', 'AboutDialog', 'PreflightSheet',
      'DbPasswordDialog', 'HostKeyDialog', 'SshPasswordDialog',
    ]) {
      expect(screen.getAllByTestId(`slot-${name}`), name).toHaveLength(1);
    }
    expect(document.documentElement.style.getPropertyValue('--sheet-top')).toBe('16px');

    await act(async () => {
      await useWorkspaceStore.getState().openWorkspace('c-prod');
    });
    expect(screen.queryByTestId('slot-WelcomeScreen')).toBeNull();
    expect(screen.getByRole('toolbar')).toBeInTheDocument();
    expect(screen.getByRole('tablist', { name: '탭' })).toBeInTheDocument();
    expect(screen.getByTestId('slot-Sidebar')).toBeInTheDocument();
    expect(screen.getByTestId('slot-JobChip-toolbar')).toBeInTheDocument();
    // The status pill names the connection and its tag.
    expect(screen.getByRole('button', { name: /prod \(PostgreSQL\)/ })).toHaveTextContent('production');
    // Inspector closed by default.
    expect(screen.queryByTestId('slot-ActivityPanel')).toBeNull();
    const firstTab = useTabsStore.getState().sets['c-prod'].ids[0];
    expect(screen.getByTestId(`slot-QueryTabView-${firstTab}`)).toBeInTheDocument();
    expect(document.documentElement.style.getPropertyValue('--sheet-top')).toBe('48px');

    act(() => useWorkspaceStore.getState().showWelcome());
    expect(screen.getByTestId('slot-WelcomeScreen')).toBeInTheDocument();
  });

  it('keeps query tabs mounted (hidden when inactive) and shows the inspector and console frames on demand', async () => {
    const user = userEvent.setup();
    render(<AppShell />);
    await act(async () => {
      await useWorkspaceStore.getState().openWorkspace('c-prod');
    });
    const a = useTabsStore.getState().sets['c-prod'].ids[0];
    let b = '';
    act(() => {
      b = useTabsStore.getState().openQuery('c-prod');
    });
    expect(screen.getByTestId(`slot-QueryTabView-${a}`).closest('[role="tabpanel"]')).toHaveAttribute('hidden');
    expect(screen.getByTestId(`slot-QueryTabView-${b}`).closest('[role="tabpanel"]')).not.toHaveAttribute('hidden');

    await user.click(screen.getByRole('button', { name: '인스펙터' }));
    expect(screen.getByTestId('slot-ActivityPanel')).toBeInTheDocument();
    act(() => useUiStore.getState().setLogOpen(true));
    expect(screen.getByTestId('slot-ConsoleDrawer')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '사이드바' }));
    expect(screen.queryByTestId('slot-Sidebar')).toBeNull();
  });

  it('closing the console acknowledges the error flag', async () => {
    const user = userEvent.setup();
    render(<AppShell />);
    act(() => {
      useLogsStore.setState({ errorFlag: true });
      useUiStore.getState().setLogOpen(true);
    });
    await user.click(screen.getByRole('button', { name: 'close console' }));
    expect(useUiStore.getState().logOpen).toBe(false);
    expect(useLogsStore.getState().errorFlag).toBe(false);
  });

  it('keeps <html lang> on the UI language', () => {
    render(<AppShell />);
    expect(document.documentElement.lang).toBe('ko');
    act(() => useUiStore.getState().setLang('en'));
    expect(document.documentElement.lang).toBe('en');
    act(() => useUiStore.getState().setLang('ko'));
    expect(document.documentElement.lang).toBe('ko');
  });

  it('F5 / Ctrl+R / Ctrl+P never reach the WebView, also inside a dialog, where they run nothing', () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    render(<AppShell />);
    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    const field = document.createElement('input');
    dialog.appendChild(field);
    document.body.appendChild(dialog);
    const press = (init: KeyboardEventInit) => {
      const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
      act(() => {
        field.dispatchEvent(e);
      });
      return e.defaultPrevented;
    };
    mockBackend.calls.length = 0;
    expect(press({ key: 'F5', code: 'F5' })).toBe(true);
    expect(press({ key: 'r', code: 'KeyR', ctrlKey: true })).toBe(true);
    expect(press({ key: 'R', code: 'KeyR', ctrlKey: true, shiftKey: true })).toBe(true);
    expect(press({ key: 'p', code: 'KeyP', ctrlKey: true })).toBe(true);
    // Inside the dialog: no refresh, no palette.
    expect(mockBackend.calls).not.toContain('conn_list');
    expect(useUiStore.getState().palette.open).toBe(false);
    // Ordinary editing keys stay with the field.
    expect(press({ key: 'c', code: 'KeyC', ctrlKey: true })).toBe(false);
    dialog.remove();
    vi.restoreAllMocks();
  });
});
