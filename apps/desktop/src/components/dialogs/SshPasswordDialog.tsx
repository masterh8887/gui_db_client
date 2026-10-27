'use client';

// `tunnel:password`: ssh asks for a password or key passphrase. Sent once, never stored.

import { useState } from 'react';
import { tunnelProvideSecret } from '@/lib/commands';
import { t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Dialog, Field, Input } from '@/components/ui';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

export function SshPasswordDialog() {
  const prompt = useUiStore((s) => s.sshPasswordPrompts[0]);
  const resolve = useUiStore((s) => s.resolveSshPasswordPrompt);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  if (!prompt) return null;

  const submit = async (value: string | undefined) => {
    setBusy(true);
    try {
      await tunnelProvideSecret(prompt.promptId, value);
    } catch (e) {
      logLocal('error', `tunnel_provide_secret: ${errorText(e)}`);
    } finally {
      setSecret('');
      setBusy(false);
      resolve(prompt.promptId);
    }
  };

  const label = t(prompt.kind === 'passphrase' ? 'sshpw.passphrase' : 'sshpw.password', { user: prompt.user, host: prompt.host });
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && void submit(undefined)}
      title={t('sshpw.title')}
      width="sm"
      modal
      footer={
        <>
          <Button variant="ghost" onClick={() => void submit(undefined)} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button variant="primary" onClick={() => void submit(secret)} loading={busy} disabled={!secret}>
            {t('sshpw.submit')}
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
        <Field label={label} htmlFor="ssh-secret" hint={t('sshpw.hint')}>
          <Input id="ssh-secret" type="password" autoComplete="off" autoFocus value={secret} onChange={(e) => setSecret(e.target.value)} />
        </Field>
      </form>
    </Dialog>
  );
}
