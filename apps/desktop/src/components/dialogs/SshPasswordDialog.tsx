'use client';

// `tunnel:password` (DESIGN.md §5.I "Prompts"): ssh asks for a password or key passphrase. Sent
// once, never stored. Esc, scrim and 취소 send the cancel answer (`tunnel_provide_secret` without
// a secret); the prompt is resolved in `finally`. Queue: oldest first.

import type { SshPasswordPrompt } from 'ipc-types';
import { useState } from 'react';
import { tunnelProvideSecret } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { Button, Field, Input, Sheet } from '@/components/ui';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { promptContext } from './promptContext';
import { sp } from './SshPasswordDialog.strings';

export function SshPasswordDialog() {
  const prompt = useUiStore((s) => s.sshPasswordPrompts[0]);
  if (!prompt) return null;
  // Keyed so every prompt starts with an empty field.
  return <SshPasswordSheet key={prompt.promptId} prompt={prompt} />;
}

function SshPasswordSheet({ prompt }: { prompt: SshPasswordPrompt }) {
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);

  const submit = async (value: string | undefined) => {
    if (busy) return;
    setBusy(true);
    try {
      await tunnelProvideSecret(prompt.promptId, value);
    } catch (e) {
      logLocal('error', `tunnel_provide_secret: ${errorText(e)}`);
    } finally {
      setSecret('');
      setBusy(false);
      useUiStore.getState().resolveSshPasswordPrompt(prompt.promptId);
    }
  };

  const label = sp(prompt.kind === 'passphrase' ? 'passphrase' : 'password', { user: prompt.user, host: prompt.host });
  return (
    <Sheet
      open
      width="sm"
      title={sp('title')}
      description={promptContext(prompt.connName, prompt.jobLabel)}
      dismissible={!busy}
      onOpenChange={(o) => {
        if (!o) void submit(undefined);
      }}
      footer={
        <>
          <Button variant="secondary" onClick={() => void submit(undefined)} disabled={busy}>
            {sp('cancel')}
          </Button>
          <Button variant="primary" onClick={() => void submit(secret)} loading={busy} disabled={!secret}>
            {sp('submit')}
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (secret) void submit(secret);
        }}
      >
        <Field label={label} htmlFor="ssh-secret" hint={sp('hint')}>
          <Input id="ssh-secret" type="password" autoComplete="off" autoFocus value={secret} onChange={(e) => setSecret(e.target.value)} />
        </Field>
      </form>
    </Sheet>
  );
}
