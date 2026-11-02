import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { setLang } from '@/lib/i18n';
import { mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { AboutDialog, aboutText, middleTruncate, platformName } from './AboutDialog';

beforeAll(() => setLang('en'));
afterAll(() => setLang('ko'));

function renderDialog() {
  useUiStore.setState({ aboutOpen: true });
  return render(
    <TooltipProvider>
      <AboutDialog />
    </TooltipProvider>,
  );
}

describe('AboutDialog', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
  });
  afterEach(() => vi.restoreAllMocks());

  it('shows the app info from the backend and loads the driver summary', async () => {
    renderDialog();
    expect(await screen.findByText('0.1.0-mock')).toBeInTheDocument();
    expect(screen.getByText('Browser (mock)')).toBeInTheDocument();
    expect(screen.getByText('wasm32')).toBeInTheDocument();
    expect(screen.getAllByText('(in-browser mock)')).toHaveLength(2);
    expect(mockBackend.calls).toContain('app_info');

    // The store had no drivers: the dialog asked for them.
    expect(await screen.findByText('Oracle Database')).toBeInTheDocument();
    expect(mockBackend.calls).toContain('drivers_list');
    expect(screen.getByText('5 of 6 available')).toBeInTheDocument();
    const drivers = screen.getByRole('region', { name: 'Drivers' });
    expect(within(drivers).getAllByText('Available')).toHaveLength(5);
    expect(within(drivers).getByText('Not installed')).toBeInTheDocument();
    expect(within(drivers).getByText('Vendor client')).toBeInTheDocument();
  });

  it('uses the drivers already in the store', async () => {
    useConnectionsStore.setState({ drivers: mockDrivers().slice(0, 2) });
    renderDialog();
    await screen.findByText('0.1.0-mock');
    expect(screen.getByText('2 of 2 available')).toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('drivers_list');
  });

  it('opens the config and log folders', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByText('0.1.0-mock');
    await user.click(screen.getByRole('button', { name: 'Open folder: Config folder' }));
    await user.click(screen.getByRole('button', { name: 'Open folder: Log folder' }));
    await waitFor(() => expect(mockBackend.calls.filter((c) => c === 'open_path')).toHaveLength(2));
  });

  it('creates a diagnostics bundle in the chosen folder and shows its path', async () => {
    const user = userEvent.setup();
    const prompt = vi.spyOn(window, 'prompt').mockReturnValue('C:\\support');
    renderDialog();
    await user.click(screen.getByRole('button', { name: 'Create diagnostics bundle' }));
    const path = await screen.findByTestId('bundle-path');
    expect(prompt).toHaveBeenCalled();
    expect(path.textContent).toMatch(/^C:\\support\\bulkdb-diagnostics-.*\.zip$/);
    expect(mockBackend.calls).toContain('diagnostics_bundle');

    // Cancelling the folder picker does nothing.
    prompt.mockReturnValue(null);
    mockBackend.calls.length = 0;
    await user.click(screen.getByRole('button', { name: 'Create diagnostics bundle' }));
    expect(mockBackend.calls).not.toContain('diagnostics_bundle');
  });

  it('reads ui.aboutOpen (no props) and closes through it', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByText('0.1.0-mock');
    const dialog = screen.getByRole('dialog', { name: 'About BulkDB' });
    await user.click(within(dialog).getByRole('button', { name: 'Close' }));
    expect(useUiStore.getState().aboutOpen).toBe(false);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('closes with Esc', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByText('0.1.0-mock');
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().aboutOpen).toBe(false);
  });

  it('renders nothing when closed', () => {
    render(<AboutDialog />);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(mockBackend.calls).not.toContain('app_info');
  });

  it('copies the support summary and says so', async () => {
    const user = userEvent.setup();
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    renderDialog();
    await screen.findByText('0.1.0-mock');
    await user.click(screen.getByRole('button', { name: /Copy info/ }));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(expect.stringMatching(/^BulkDB 0\.1\.0-mock\n/)));
    expect(await screen.findAllByText('Copied')).not.toHaveLength(0);
  });

  it('formats the support summary', () => {
    const text = aboutText(
      { version: '1.2.3', platform: 'macos', arch: 'aarch64', configDir: '/cfg', logDir: '/logs' },
      mockDrivers().filter((d) => d.id === 'oracle' || d.id === 'sqlite'),
    );
    expect(text.split('\n')).toEqual([
      'BulkDB 1.2.3',
      'macOS aarch64',
      'config: /cfg',
      'logs: /logs',
      'driver sqlite (builtin): available',
      expect.stringMatching(/^driver oracle \(vendor-client\): missing - oci\.dll not found/),
    ]);
    expect(platformName('windows')).toBe('Windows');
    expect(platformName('freebsd')).toBe('freebsd');
  });

  it('shortens long paths in the middle', () => {
    expect(middleTruncate('/short')).toBe('/short');
    const long = 'C:\\Users\\someone\\AppData\\Roaming\\BulkDB\\config\\deeply\\nested\\folder';
    const out = middleTruncate(long, 30);
    expect(out).toHaveLength(30);
    expect(out.startsWith('C:\\Users')).toBe(true);
    expect(out.endsWith('folder')).toBe(true);
    expect(out).toContain('…');
  });
});
