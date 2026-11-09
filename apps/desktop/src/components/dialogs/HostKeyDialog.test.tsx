import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { HostKeyPrompt } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { tunnelConfirmHostkey } from '@/lib/commands';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { HostKeyDialog } from './HostKeyDialog';
import { hs } from './HostKeyDialog.strings';

vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, tunnelConfirmHostkey: vi.fn(async () => undefined) };
});

const FIRST: HostKeyPrompt = {
  promptId: 'p1',
  host: 'bastion.internal',
  keyType: 'ssh-ed25519',
  fingerprint: 'SHA256:abcdefghijklmnop',
  message: "The authenticity of host 'bastion.internal' can't be established.\nED25519 key fingerprint is SHA256:abcdefghijklmnop.",
};
const CHANGED: HostKeyPrompt = {
  promptId: 'p2',
  host: 'db.internal',
  fingerprint: 'SHA256:zzz',
  message: '@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@',
};

function renderDialog() {
  return render(
    <TooltipProvider>
      <HostKeyDialog />
    </TooltipProvider>,
  );
}

describe('HostKeyDialog', () => {
  beforeEach(() => {
    resetStores();
    vi.mocked(tunnelConfirmHostkey).mockClear();
  });

  it('shows host, key type, fingerprint and the raw ssh message; accept answers true', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushHostKeyPrompt(FIRST);
    renderDialog();
    const sheet = screen.getByRole('dialog', { name: hs('title') });
    expect(sheet).toHaveTextContent(hs('body', { host: 'bastion.internal' }));
    expect(screen.getByText('ssh-ed25519')).toBeInTheDocument();
    expect(screen.getByText('SHA256:abcdefghijklmnop')).toBeInTheDocument();
    expect(screen.getByLabelText(hs('message'))).toHaveTextContent("can't be established");
    await user.click(screen.getByRole('button', { name: hs('accept') }));
    expect(tunnelConfirmHostkey).toHaveBeenCalledWith('p1', true);
    await waitFor(() => expect(useUiStore.getState().hostKeyPrompts).toHaveLength(0));
  });

  it('warns loudly when the host key changed', () => {
    useUiStore.getState().pushHostKeyPrompt(CHANGED);
    renderDialog();
    expect(screen.getByRole('alert')).toHaveTextContent(hs('changed', { host: 'db.internal' }));
    expect(screen.getByLabelText(hs('message'))).toHaveTextContent('REMOTE HOST IDENTIFICATION HAS CHANGED');
  });

  it('Esc answers false', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushHostKeyPrompt(FIRST);
    renderDialog();
    await user.keyboard('{Escape}');
    expect(tunnelConfirmHostkey).toHaveBeenCalledWith('p1', false);
    await waitFor(() => expect(useUiStore.getState().hostKeyPrompts).toHaveLength(0));
  });

  it('a scrim click answers false', async () => {
    useUiStore.getState().pushHostKeyPrompt(FIRST);
    renderDialog();
    // Radix dismisses on a pointerdown outside the content (the scrim); its listener attaches
    // on the next tick after opening.
    await act(() => new Promise((r) => setTimeout(r, 0)));
    const scrim = document.querySelector('.fixed.inset-0') ?? document.body;
    fireEvent.pointerDown(scrim);
    fireEvent.pointerUp(scrim);
    fireEvent.click(scrim);
    await waitFor(() => expect(tunnelConfirmHostkey).toHaveBeenCalledWith('p1', false));
  });

  it('reject answers false and the queue continues with the next prompt, oldest first', async () => {
    const user = userEvent.setup();
    useUiStore.getState().pushHostKeyPrompt(FIRST);
    useUiStore.getState().pushHostKeyPrompt(CHANGED);
    renderDialog();
    expect(screen.getByText(hs('more', { n: 1 }))).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: hs('reject') }));
    expect(tunnelConfirmHostkey).toHaveBeenCalledWith('p1', false);
    expect(await screen.findByText('SHA256:zzz')).toBeInTheDocument();
  });

  it('resolves the prompt even when the answer fails', async () => {
    const user = userEvent.setup();
    vi.mocked(tunnelConfirmHostkey).mockRejectedValueOnce(new Error('prompt gone'));
    useUiStore.getState().pushHostKeyPrompt(FIRST);
    renderDialog();
    await user.click(screen.getByRole('button', { name: hs('accept') }));
    await waitFor(() => expect(useUiStore.getState().hostKeyPrompts).toHaveLength(0));
  });
});
