'use client';

// Password for a connection saved with "ask every time" (`savePassword = false`). The password
// goes to the backend once, through `conn_test`; the backend keeps it in memory for the rest of
// the session and the UI forgets it immediately. On success the action that needed it resumes.

import { useState } from 'react';
import { t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Dialog, Field, Input, Notice } from '@/components/ui';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { logLocal } from '@/stores/logs';
import { useUiStore, type DbPasswordPrompt } from '@/stores/ui';

/** Continue whatever was waiting for the password. */
function resume(prompt: DbPasswordPrompt): void {
  switch (prompt.reason) {
    case 'browse': {
      const catalog = useCatalogStore.getState();
      catalog.setExpanded(prompt.connId, true);
      void catalog.loadDatabases(prompt.connId).catch(() => undefined);
      break;
    }
    case 'run':
      void useExtractStore.getState().run();
      break;
    case 'explain':
      void useExtractStore.getState().explain();
      break;
    case 'test':
      break;
  }
}

export function DbPasswordDialog() {
  const prompt = useUiStore((s) => s.dbPasswordPrompt);
  if (!prompt) return null;
  // Keyed so a new request always starts with an empty field and no stale error.
  return <DbPasswordForm key={`${prompt.connId}:${prompt.reason}`} prompt={prompt} />;
}

function DbPasswordForm({ prompt }: { prompt: DbPasswordPrompt }) {
  const close = useUiStore((s) => s.closeDbPassword);
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === prompt.connId));
  const test = useConnectionsStore((s) => s.test);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!secret || busy) return;
    setBusy(true);
    setError(null);
    const password = secret;
    setSecret('');
    try {
      await test(prompt.connId, password);
      logLocal('info', `${conn?.name ?? prompt.connId}: ${t('tree.status.ok')}`);
      close();
      resume(prompt);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && !busy && close()}
      title={t('dbpw.title')}
      width="sm"
      modal={busy}
      footer={
        <>
          <Button variant="ghost" onClick={close} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button variant="primary" onClick={() => void submit()} loading={busy} disabled={!secret}>
            {t('dbpw.submit')}
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {error && <Notice tone="danger">{error}</Notice>}
        <Field
          label={t('dbpw.label', { user: conn?.user ?? '', name: conn?.name ?? prompt.connId })}
          htmlFor="db-secret"
          hint={t('dbpw.hint')}
        >
          <Input
            id="db-secret"
            type="password"
            autoComplete="off"
            autoFocus
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
          />
        </Field>
      </form>
    </Dialog>
  );
}
