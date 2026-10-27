'use client';

// `tunnel:hostkey`: the ssh host key is unknown; the user compares the fingerprint.

import { useState } from 'react';
import { tunnelConfirmHostkey } from '@/lib/commands';
import { t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Dialog, Notice } from '@/components/ui';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

export function HostKeyDialog() {
  const prompt = useUiStore((s) => s.hostKeyPrompts[0]);
  const resolve = useUiStore((s) => s.resolveHostKeyPrompt);
  const [busy, setBusy] = useState(false);
  if (!prompt) return null;

  const answer = async (accept: boolean) => {
    setBusy(true);
    try {
      await tunnelConfirmHostkey(prompt.promptId, accept);
    } catch (e) {
      logLocal('error', `tunnel_confirm_hostkey: ${errorText(e)}`);
    } finally {
      setBusy(false);
      resolve(prompt.promptId);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && void answer(false)}
      title={t('hostkey.title')}
      modal
      footer={
        <>
          <Button variant="ghost" onClick={() => void answer(false)} disabled={busy}>
            {t('hostkey.reject')}
          </Button>
          <Button variant="primary" onClick={() => void answer(true)} loading={busy}>
            {t('hostkey.accept')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-2">
        <Notice tone="warning">{t('hostkey.body', { host: prompt.host })}</Notice>
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-[13px]">
          <dt className="text-muted-foreground">{t('conn.host')}</dt>
          <dd className="font-mono">{prompt.host}</dd>
          {prompt.keyType && (
            <>
              <dt className="text-muted-foreground">{t('hostkey.keyType')}</dt>
              <dd className="font-mono">{prompt.keyType}</dd>
            </>
          )}
          <dt className="text-muted-foreground">{t('hostkey.fingerprint')}</dt>
          <dd className="break-all font-mono">{prompt.fingerprint}</dd>
        </dl>
        <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded border border-border bg-muted p-2 font-mono text-[11px] leading-4">{prompt.message}</pre>
      </div>
    </Dialog>
  );
}
