'use client';

// Password for a connection saved with "ask every time" (DESIGN.md §5.I "Prompts", §7.1 "ui").
// Shows the head of `ui.dbPasswordPrompts`, keyed by `prompt.id`. The password goes to the
// backend once, through `conn_test`; the backend keeps it in memory for the session and the UI
// forgets it at once. On success the prompt is resolved and every request merged into it resumes
// (`resumeDbPasswordRequests`: open → workspace, browse → databases, run / explain → that tab).
// Esc, scrim and 취소 are the cancel answer: the prompt is dropped with its requests.

import { useState } from 'react';
import { resumeDbPasswordRequests } from '@/lib/db-password';
import { errorText } from '@/lib/ipc-error';
import { Button, EnvTag, Field, Input, Notice, Sheet } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useUiStore, type DbPasswordPrompt } from '@/stores/ui';
import { dp } from './DbPasswordDialog.strings';

export function DbPasswordDialog() {
  const prompt = useUiStore((s) => s.dbPasswordPrompts[0]);
  if (!prompt) return null;
  // Keyed so a new prompt always starts with an empty field and no stale error; requests merged
  // into this prompt keep it mounted.
  return <DbPasswordSheet key={prompt.id} prompt={prompt} />;
}

function DbPasswordSheet({ prompt }: { prompt: DbPasswordPrompt }) {
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === prompt.connId));
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const name = conn?.name ?? prompt.connId;

  const cancel = () => useUiStore.getState().resolveDbPassword(prompt.id);

  const submit = async () => {
    if (!secret || busy) return;
    setBusy(true);
    setError(null);
    const password = secret;
    setSecret('');
    try {
      await useConnectionsStore.getState().test(prompt.connId, password);
      logLocal('info', dp('unlocked', { name }));
      // Requests may have been merged into the prompt while the test ran: resume the latest copy.
      const latest = useUiStore.getState().dbPasswordPrompts.find((p) => p.id === prompt.id) ?? prompt;
      useUiStore.getState().resolveDbPassword(prompt.id);
      void resumeDbPasswordRequests(latest);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet
      open
      width="sm"
      title={dp('title')}
      description={
        <span className="inline-flex items-center gap-1.5">
          <EnvTag tag={conn?.tag} variant="badge" />
          <span>{name}</span>
        </span>
      }
      dismissible={!busy}
      onOpenChange={(o) => {
        if (!o) cancel();
      }}
      footer={
        <>
          <Button variant="secondary" onClick={cancel} disabled={busy}>
            {dp('cancel')}
          </Button>
          <Button variant="primary" onClick={() => void submit()} loading={busy} disabled={!secret}>
            {dp('submit')}
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {error && <Notice tone="danger">{error}</Notice>}
        <Field label={conn?.user ? dp('label', { user: conn.user, name }) : dp('labelNoUser', { name })} htmlFor="db-secret" hint={dp('hint')}>
          <Input id="db-secret" type="password" autoComplete="off" autoFocus value={secret} onChange={(e) => setSecret(e.target.value)} />
        </Field>
      </form>
    </Sheet>
  );
}
