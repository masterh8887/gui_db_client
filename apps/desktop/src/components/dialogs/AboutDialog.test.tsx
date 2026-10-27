import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { setLang } from '@/lib/i18n';
import { mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { AboutDialog, aboutText, platformName } from './AboutDialog';

beforeAll(() => setLang('en'));
afterAll(() => setLang('ko'));

function renderDialog(onOpenChange = vi.fn()) {
  return render(
    <TooltipProvider>
      <AboutDialog open onOpenChange={onOpenChange} />
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

  it('closes through onOpenChange', async () => {
    const user = userEvent.setup();
    const onOpenChange = vi.fn();
    renderDialog(onOpenChange);
    await screen.findByText('0.1.0-mock');
    const dialog = screen.getByRole('dialog');
    await user.click(within(dialog).getAllByRole('button', { name: 'Close' }).at(-1)!);
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it('renders nothing when closed', () => {
    render(<AboutDialog open={false} onOpenChange={vi.fn()} />);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(mockBackend.calls).not.toContain('app_info');
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
});
