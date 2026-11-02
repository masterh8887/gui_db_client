import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { tunnelProvideSecret } from '@/lib/commands';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { SshPasswordDialog } from './SshPasswordDialog';
import { sp } from './SshPasswordDialog.strings';

vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, tunnelProvideSecret: vi.fn(async () => undefined) };
});

function renderDialog() {
  return render(
    <TooltipProvider>
      <SshPasswordDialog />
    </TooltipProvider>,
  );
}

describe('SshPasswordDialog', () => {
  beforeEach(() => {
    resetStores();
    vi.mocked(tunnelProvideSecret).mockClear();
  });

  it('sends the typed password once and resolves the prompt', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushSshPasswordPrompt({ promptId: 's1', host: 'bastion', user: 'deploy', kind: 'password' });
    renderDialog();
    const field = screen.getByLabelText(sp('password', { user: 'deploy', host: 'bastion' }));
    expect(screen.getByRole('button', { name: sp('submit') })).toBeDisabled();
    await user.type(field, 'hunter2{Enter}');
    expect(tunnelProvideSecret).toHaveBeenCalledWith('s1', 'hunter2');
    await waitFor(() => expect(useUiStore.getState().sshPasswordPrompts).toHaveLength(0));
  });

  it('asks for a key passphrase', () => {
    useUiStore.getState().pushSshPasswordPrompt({ promptId: 's2', host: 'bastion', user: 'deploy', kind: 'passphrase' });
    renderDialog();
    expect(screen.getByLabelText(sp('passphrase', { user: 'deploy', host: 'bastion' }))).toBeInTheDocument();
  });

  it('Esc sends the cancel answer (no secret)', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushSshPasswordPrompt({ promptId: 's3', host: 'bastion', user: 'deploy', kind: 'password' });
    renderDialog();
    await user.keyboard('{Escape}');
    expect(tunnelProvideSecret).toHaveBeenCalledWith('s3', undefined);
    await waitFor(() => expect(useUiStore.getState().sshPasswordPrompts).toHaveLength(0));
  });

  it('starts the next prompt with an empty field', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushSshPasswordPrompt({ promptId: 'a', host: 'h1', user: 'u', kind: 'password' });
    useUiStore.getState().pushSshPasswordPrompt({ promptId: 'b', host: 'h2', user: 'u', kind: 'password' });
    renderDialog();
    await user.type(screen.getByLabelText(sp('password', { user: 'u', host: 'h1' })), 'one');
    await user.click(screen.getByRole('button', { name: sp('submit') }));
    const next = await screen.findByLabelText(sp('password', { user: 'u', host: 'h2' }));
    expect(next).toHaveValue('');
  });
});
