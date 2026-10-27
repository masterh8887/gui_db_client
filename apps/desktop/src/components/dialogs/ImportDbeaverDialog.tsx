'use client';

// Review and save connections read from DBeaver's data-sources.json (`conn_import_dbeaver`).
// The backend only parses the file; rows become connections when the user imports them here.
// Passwords are never imported (DBeaver encrypts them; BulkDB keeps secrets in the keyring).

import type { ImportedConnection } from 'ipc-types';
import { Check } from 'lucide-react';
import { useState } from 'react';
import { t, type I18nKey } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Badge, Button, Checkbox, Dialog, Notice } from '@/components/ui';
import { importRowStatus, useConnectionsStore, type ImportRowStatus, type ImportSaveResult } from '@/stores/connections';
import { logLocal } from '@/stores/logs';

/** What the tree hands over after the file was picked and parsed (or failed to parse). */
export interface ImportDbeaverSource {
  path: string;
  rows: ImportedConnection[];
  /** Set when `conn_import_dbeaver` failed; `rows` is then empty. */
  error?: string;
}

const STATUS_LABEL: Record<Exclude<ImportRowStatus, 'ok'>, { key: I18nKey; tone: 'danger' | 'warning' | 'neutral' }> = {
  unsupported: { key: 'import.unsupported', tone: 'danger' },
  driverMissing: { key: 'import.driverMissing', tone: 'warning' },
  exists: { key: 'import.exists', tone: 'neutral' },
};

function target(row: ImportedConnection): string {
  const c = row.connection;
  if (c.fields.path) return c.fields.path;
  const hostPort = c.host ? `${c.host}${c.port ? `:${c.port}` : ''}` : '';
  const db = c.database ?? c.fields.service_name ?? '';
  return [hostPort, db].filter(Boolean).join(' / ');
}

export function ImportDbeaverDialog({ source, onClose }: { source: ImportDbeaverSource; onClose: () => void }) {
  const drivers = useConnectionsStore((s) => s.drivers);
  const connections = useConnectionsStore((s) => s.connections);
  const saveImported = useConnectionsStore((s) => s.saveImported);
  // Status is fixed at open time so rows saved here do not turn into "already exists".
  const [statuses] = useState(() => source.rows.map((r) => importRowStatus(r, drivers, connections)));
  const [selected, setSelected] = useState<boolean[]>(() => statuses.map((st) => st === 'ok' || st === 'driverMissing'));
  const [done, setDone] = useState<boolean[]>(() => source.rows.map(() => false));
  const [failures, setFailures] = useState<ImportSaveResult['failed']>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(source.error ?? null);

  const count = selected.filter((v, i) => v && !done[i]).length;

  const onImport = async () => {
    const picked = source.rows.map((row, i) => ({ row, i })).filter(({ i }) => selected[i] && !done[i]);
    if (picked.length === 0) return;
    setBusy(true);
    setError(null);
    try {
      const res = await saveImported(picked.map((p) => p.row));
      const failedAt = new Set(res.failed.map((f) => f.index));
      const nowDone = [...done];
      picked.forEach(({ i }, k) => {
        if (!failedAt.has(k)) nowDone[i] = true;
      });
      setDone(nowDone);
      setFailures(res.failed);
      if (res.saved.length > 0) logLocal('info', t('import.done', { count: res.saved.length }));
      for (const f of res.failed) logLocal('error', `conn_save '${f.name}': ${f.error}`);
      if (res.failed.length === 0) onClose();
    } catch (e) {
      // Saving reports per row; this is the `conn_list` reload afterwards failing.
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && !busy && onClose()}
      title={t('import.title')}
      width="xl"
      modal={busy}
      footer={
        <>
          <Button variant="ghost" onClick={onClose} disabled={busy}>
            {t('common.close')}
          </Button>
          <Button variant="primary" onClick={() => void onImport()} loading={busy} disabled={busy || count === 0}>
            {t('import.save', { count })}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-2 text-[13px]">
        <div className="truncate text-xs text-muted-foreground" title={source.path}>
          {t('import.file')}: {source.path}
        </div>
        {error && <Notice tone="danger">{error}</Notice>}
        {failures.length > 0 && (
          <Notice tone="danger" title={t('import.failed', { count: failures.length })}>
            <ul className="text-xs">
              {failures.map((f) => (
                <li key={`${f.index}-${f.name}`}>
                  <b>{f.name}</b>: {f.error}
                </li>
              ))}
            </ul>
          </Notice>
        )}
        {!source.error && source.rows.length === 0 && <div className="py-4 text-center text-muted-foreground">{t('import.empty')}</div>}
        {source.rows.length > 0 && (
          <>
            <Notice tone="info">{t('import.passwordNote')}</Notice>
            <table className="w-full table-fixed border-collapse">
              <thead>
                <tr className="h-7 border-b border-border text-left text-xs text-muted-foreground">
                  <th className="w-7" />
                  <th className="w-[26%] font-medium">{t('conn.name')}</th>
                  <th className="w-[12%] font-medium">{t('conn.driver')}</th>
                  <th className="w-[30%] font-medium">{t('conn.host')}</th>
                  <th className="font-medium" />
                </tr>
              </thead>
              <tbody>
                {source.rows.map((row, i) => {
                  const st = statuses[i];
                  const label = st === 'ok' ? undefined : STATUS_LABEL[st];
                  return (
                    <tr key={`${row.connection.id}-${i}`} className="h-7 border-b border-border/60 align-middle" data-status={st}>
                      <td>
                        <Checkbox
                          aria-label={row.connection.name}
                          checked={selected[i] || done[i]}
                          disabled={busy || done[i] || st === 'unsupported'}
                          onChange={(v) => setSelected((s) => s.map((x, j) => (j === i ? v : x)))}
                        />
                      </td>
                      <td className="truncate" title={row.connection.name}>
                        {row.connection.name}
                      </td>
                      <td className="truncate">{row.connection.driver || '—'}</td>
                      <td className="truncate text-xs text-muted-foreground" title={target(row)}>
                        {target(row) || '—'}
                      </td>
                      <td className="truncate text-xs" title={row.notes.join('\n')}>
                        <span className="inline-flex items-center gap-1">
                          {done[i] && (
                            <Badge tone="success" aria-label={t('common.ok')}>
                              <Check size={11} />
                            </Badge>
                          )}
                          {label && <Badge tone={label.tone}>{t(label.key)}</Badge>}
                          <span className="truncate text-muted-foreground">{row.notes.join(' · ')}</span>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </>
        )}
      </div>
    </Dialog>
  );
}
