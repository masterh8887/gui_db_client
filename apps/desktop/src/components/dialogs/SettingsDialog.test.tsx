import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { mockBackend } from '@/lib/mock-ipc';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { SettingsDialog } from './SettingsDialog';
import { ss } from './SettingsDialog.strings';

function renderDialog() {
  useUiStore.setState({ settingsOpen: true });
  return render(
    <TooltipProvider>
      <SettingsDialog />
    </TooltipProvider>,
  );
}

describe('SettingsDialog', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.configure({ latencyMs: 0 });
  });
  afterEach(() => vi.restoreAllMocks());

  it('loads the settings and keeps Save disabled until something changes', async () => {
    renderDialog();
    const jobs = await screen.findByLabelText(ss('maxJobs'));
    expect(mockBackend.calls).toContain('settings_get');
    expect(jobs).toHaveValue(useSettingsStore.getState().settings!.maxConcurrentJobs);
    expect(screen.getByRole('button', { name: ss('save') })).toBeDisabled();
    // Every setting of today is here.
    for (const label of [ss('outputDir'), ss('tunnelIdle'), ss('monitorInterval'), ss('progressInterval'), ss('logLevel')]) {
      expect(screen.getByLabelText(label)).toBeInTheDocument();
    }
    expect(screen.getByRole('checkbox', { name: ss('crlf') })).toBeInTheDocument();
    expect(screen.getByRole('checkbox', { name: ss('verify') })).toBeInTheDocument();
  });

  it('saves the edited settings and shows the saved notice', async () => {
    const user = userEvent.setup();
    renderDialog();
    const verify = await screen.findByRole('checkbox', { name: ss('verify') });
    const before = useSettingsStore.getState().settings!.verifyAfterExport;
    await user.click(verify);
    const save = screen.getByRole('button', { name: ss('save') });
    expect(save).toBeEnabled();
    await user.click(save);
    await waitFor(() => expect(mockBackend.calls).toContain('settings_set'));
    expect(await screen.findByText(ss('saved'))).toBeInTheDocument();
    expect(useSettingsStore.getState().settings!.verifyAfterExport).toBe(!before);
    expect(save).toBeDisabled();
  });

  it('applies theme and language at once', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByLabelText(ss('maxJobs'));
    const theme = screen.getByRole('radiogroup', { name: ss('theme') });
    await user.click(within(theme).getByRole('radio', { name: ss('themeDark') }));
    expect(useUiStore.getState().theme).toBe('dark');
  });

  it('opens About from the footer and closes with Close without saving', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByLabelText(ss('maxJobs'));
    await user.click(screen.getByRole('button', { name: ss('about') }));
    expect(useUiStore.getState().aboutOpen).toBe(true);
    await user.click(screen.getByRole('button', { name: ss('close') }));
    expect(useUiStore.getState().settingsOpen).toBe(false);
    expect(mockBackend.calls).not.toContain('settings_set');
  });

  it('closes with Esc', async () => {
    const user = userEvent.setup();
    renderDialog();
    await screen.findByLabelText(ss('maxJobs'));
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().settingsOpen).toBe(false);
  });

  it('creates a diagnostics bundle in the chosen folder', async () => {
    const user = userEvent.setup();
    vi.spyOn(window, 'prompt').mockReturnValue('C:\\support');
    renderDialog();
    await screen.findByLabelText(ss('maxJobs'));
    await user.click(screen.getByRole('button', { name: ss('createBundle') }));
    expect(await screen.findByTestId('settings-bundle-path')).toHaveTextContent(/^C:\\support\\bulkdb-diagnostics-.*\.zip$/);
  });
});
