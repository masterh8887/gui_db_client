'use client';

// `tunnel:hostkey` (DESIGN.md §5.I "Prompts"): ssh does not know the server's host key (or it
// changed). The raw ssh message tells a first connection from "REMOTE HOST IDENTIFICATION HAS
// CHANGED". Esc, scrim and 거부 all answer `tunnel_confirm_hostkey(promptId, false)`; the prompt
// is resolved in `finally`. Queue: oldest first.

import type { HostKeyPrompt } from 'ipc-types';
import { useState } from 'react';
import { tunnelConfirmHostkey } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { Button, Notice, Sheet } from '@/components/ui';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { hs } from './HostKeyDialog.strings';
import { promptContext } from './promptContext';

const CHANGED = /REMOTE HOST IDENTIFICATION HAS CHANGED/i;

export function HostKeyDialog() {
  const prompt = useUiStore((s) => s.hostKeyPrompts[0]);
  const waiting = useUiStore((s) => Math.max(0, s.hostKeyPrompts.length - 1));
  if (!prompt) return null;
  return <HostKeySheet key={prompt.promptId} prompt={prompt} waiting={waiting} />;
}

function HostKeySheet({ prompt, waiting }: { prompt: HostKeyPrompt; waiting: number }) {
  const [busy, setBusy] = useState(false);
  const changed = CHANGED.test(prompt.message);

  const answer = async (accept: boolean) => {
    if (busy) return;
    setBusy(true);
    try {
      await tunnelConfirmHostkey(prompt.promptId, accept);
    } catch (e) {
      logLocal('error', `tunnel_confirm_hostkey: ${errorText(e)}`);
    } finally {
      setBusy(false);
      useUiStore.getState().resolveHostKeyPrompt(prompt.promptId);
    }
  };

  return (
    <Sheet
      open
      width="sm"
      title={hs('title')}
      description={promptContext(prompt.connName, prompt.jobLabel)}
      dismissible={!busy}
      onOpenChange={(o) => {
        if (!o) void answer(false);
      }}
      footerStart={waiting > 0 ? <span className="text-sm text-fg-secondary">{hs('more', { n: waiting })}</span> : undefined}
      footer={
        <>
          <Button variant="secondary" onClick={() => void answer(false)} disabled={busy} autoFocus>
            {hs('reject')}
          </Button>
          <Button variant="primary" onClick={() => void answer(true)} loading={busy}>
            {hs('accept')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        <Notice tone={changed ? 'danger' : 'warning'}>{hs(changed ? 'changed' : 'body', { host: prompt.host })}</Notice>
        <dl data-selectable className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 rounded-md bg-panel px-3 py-2 text-sm">
          <dt className="text-fg-secondary">{hs('host')}</dt>
          <dd className="min-w-0 break-all font-mono">{prompt.host}</dd>
          {prompt.keyType && (
            <>
              <dt className="text-fg-secondary">{hs('keyType')}</dt>
              <dd className="min-w-0 break-all font-mono">{prompt.keyType}</dd>
            </>
          )}
          <dt className="text-fg-secondary">{hs('fingerprint')}</dt>
          <dd className="min-w-0 break-all font-mono">{prompt.fingerprint}</dd>
        </dl>
        <pre
          data-selectable
          aria-label={hs('message')}
          tabIndex={0}
          className="max-h-40 overflow-auto rounded-md bg-panel px-3 py-2 font-mono text-sm whitespace-pre-wrap break-words text-fg-secondary outline-none focus-visible:shadow-[inset_0_0_0_2px_var(--focus)]"
        >
          {prompt.message}
        </pre>
      </div>
    </Sheet>
  );
}
