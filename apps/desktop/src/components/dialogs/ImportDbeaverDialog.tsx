'use client';

// DBeaver import (DESIGN.md §5.I "ImportDbeaverFlow"): when `ui.importDbeaverOpen` turns on, pick
// DBeaver's data-sources.json, parse it (`conn_import_dbeaver`; nothing is saved yet) and review
// the rows here. Rows become connections only when the user imports them. Passwords are never
// imported (DBeaver encrypts them; BulkDB keeps secrets in the keyring).

import type { ImportedConnection } from 'ipc-types';
import { Check } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { pickAnyFile } from '@/lib/dialogs';
import { errorText } from '@/lib/ipc-error';
import { toast } from '@/lib/toast';
import { Badge, Button, Checkbox, Dialog, Notice, Spinner, Tooltip } from '@/components/ui';
import { DriverBadge } from '@/components/welcome/DriverBadge';
import { MiddleTruncate } from '@/components/welcome/MiddleTruncate';
import { importRowStatus, useConnectionsStore, type ImportRowStatus, type ImportSaveResult } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { is } from './ImportDbeaverDialog.strings';

/** A picked and parsed (or failed) file. */
export interface ImportDbeaverSource {
  path: string;
  rows: ImportedConnection[];
  /** Set when `conn_import_dbeaver` failed; `rows` is then empty. */
  error?: string;
}

const STATUS: Record<ImportRowStatus, { label: () => string; tip: () => string | null; tone: 'success' | 'danger' | 'warning' | 'neutral' }> = {
  ok: { label: () => is('statusOk'), tip: () => null, tone: 'success' },
  unsupported: { label: () => is('statusUnsupported'), tip: () => is('statusUnsupportedTip'), tone: 'danger' },
  driverMissing: { label: () => is('statusDriverMissing'), tip: () => is('statusDriverMissingTip'), tone: 'warning' },
  exists: { label: () => is('statusExists'), tip: () => is('statusExistsTip'), tone: 'neutral' },
};

function target(row: ImportedConnection): string {
  const c = row.connection;
  if (c.fields.path) return c.fields.path;
  const hostPort = c.host ? `${c.host}${c.port ? `:${c.port}` : ''}` : '';
  const db = c.database ?? c.fields.service_name ?? '';
  return [hostPort, db].filter(Boolean).join(' / ');
}

/** The slot (DESIGN.md §7.6): mounted once, active while `ui.importDbeaverOpen`. */
export function ImportDbeaverFlow() {
  const open = useUiStore((s) => s.importDbeaverOpen);
  if (!open) return null;
  // Mounted per open, so closing resets everything.
  return <ImportSession />;
}

function closeFlow(): void {
  useUiStore.getState().setImportDbeaverOpen(false);
}

function ImportSession() {
  const [path, setPath] = useState<string | null>(null);
  const [source, setSource] = useState<ImportDbeaverSource | null>(null);
  const started = useRef(false);
  const alive = useRef(true);

  useEffect(() => {
    alive.current = true;
    // Once per open, also under StrictMode's double effect (a second native file dialog).
    if (!started.current) {
      started.current = true;
      void (async () => {
        let picked: string | null;
        try {
          picked = await pickAnyFile([{ name: is('pickFilter'), extensions: ['json'] }]);
        } catch (e) {
          logLocal('error', `file dialog: ${errorText(e)}`);
          closeFlow();
          return;
        }
        if (!alive.current) return;
        if (!picked) {
          closeFlow();
          return;
        }
        setPath(picked);
        try {
          const rows = await useConnectionsStore.getState().importDbeaver(picked);
          if (alive.current) setSource({ path: picked, rows });
        } catch (e) {
          const error = errorText(e);
          logLocal('error', `conn_import_dbeaver: ${error}`);
          if (alive.current) setSource({ path: picked, rows: [], error });
        }
      })();
    }
    return () => {
      alive.current = false;
    };
  }, []);

  if (!path) return null;
  return <ImportReview path={path} source={source} onClose={closeFlow} />;
}

function ImportReview({ path, source, onClose }: { path: string; source: ImportDbeaverSource | null; onClose: () => void }) {
  if (source) return <ImportRows source={source} onClose={onClose} />;
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={is('title')}
      description={<MiddleTruncate text={path} />}
      width="lg"
      footer={
        <Button variant="secondary" onClick={onClose}>
          {is('cancel')}
        </Button>
      }
    >
      <div role="status" className="flex items-center justify-center gap-2 py-8 text-fg-secondary">
        <Spinner size={14} /> {is('parsing')}
      </div>
    </Dialog>
  );
}

function ImportRows({ source, onClose }: { source: ImportDbeaverSource; onClose: () => void }) {
  const drivers = useConnectionsStore((s) => s.drivers);
  const connections = useConnectionsStore((s) => s.connections);
  const saveImported = useConnectionsStore((s) => s.saveImported);
  // Status is fixed when the file is read, so rows saved here do not turn into "already exists".
  const [statuses] = useState(() => source.rows.map((r) => importRowStatus(r, drivers, connections)));
  const [selected, setSelected] = useState<boolean[]>(() => statuses.map((st) => st === 'ok' || st === 'driverMissing'));
  const [done, setDone] = useState<boolean[]>(() => source.rows.map(() => false));
  const [failures, setFailures] = useState<ImportSaveResult['failed']>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selectable = statuses.map((st, i) => st !== 'unsupported' && !done[i]);
  const count = selected.filter((v, i) => v && selectable[i]).length;
  const selectableCount = selectable.filter(Boolean).length;
  const allSelected = selectableCount > 0 && count === selectableCount;

  const onImport = async () => {
    const picked = source.rows.map((row, i) => ({ row, i })).filter(({ i }) => selected[i] && selectable[i]);
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
      if (res.saved.length > 0) {
        const msg = is('done', { count: res.saved.length });
        logLocal('info', msg);
        toast(msg, { tone: 'success' });
      }
      for (const f of res.failed) logLocal('error', `conn_save '${f.name}': ${f.error}`);
      if (res.failed.length === 0) onClose();
    } catch (e) {
      // Saving reports per row; this is the `conn_list` reload afterwards failing.
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const footer = (
    <>
      <Button variant="secondary" onClick={onClose} disabled={busy}>
        {is('close')}
      </Button>
      {source.rows.length > 0 && (
        <Button variant="primary" onClick={() => void onImport()} loading={busy} disabled={busy || count === 0}>
          {is('import', { count })}
        </Button>
      )}
    </>
  );

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && !busy && onClose()}
      title={is('title')}
      description={<MiddleTruncate text={source.path} />}
      width="lg"
      modal={busy}
      footer={footer}
    >
      <div className="flex flex-col gap-2.5">
        {source.error && (
          <Notice tone="danger" title={is('parseFailed')}>
            <span className="text-sm">{source.error}</span>
          </Notice>
        )}
        {error && (
          <Notice tone="danger" onClose={() => setError(null)}>
            {error}
          </Notice>
        )}
        {failures.length > 0 && (
          <Notice tone="danger" title={is('failed', { count: failures.length })}>
            <ul className="text-sm">
              {failures.map((f) => (
                <li key={`${f.index}-${f.name}`}>
                  <span className="font-medium">{f.name}</span>: {f.error}
                </li>
              ))}
            </ul>
            <div className="mt-1 text-sm text-fg-secondary">{is('retryHint')}</div>
          </Notice>
        )}
        {!source.error && source.rows.length === 0 && <div className="py-6 text-center text-fg-secondary">{is('empty')}</div>}
        {source.rows.length > 0 && (
          <>
            <Notice tone="info">{is('passwordNote')}</Notice>
            <div className="list-scroll max-h-[360px]">
              <table className="w-full table-fixed border-collapse">
                <thead className="sticky top-0 z-10 bg-elevated">
                  <tr className="h-(--h-row) border-b border-separator text-left text-sm text-fg-secondary">
                    <th className="w-8 px-2 font-normal">
                      <Checkbox
                        aria-label={is('selectAll')}
                        checked={allSelected}
                        disabled={busy || selectableCount === 0}
                        onChange={(v) => setSelected((s) => s.map((x, j) => (selectable[j] ? v : x)))}
                      />
                    </th>
                    <th className="w-[32%] font-normal">{is('colName')}</th>
                    <th className="w-[18%] font-normal">{is('colDriver')}</th>
                    <th className="font-normal">{is('colTarget')}</th>
                    <th className="w-[24%] font-normal">{is('colStatus')}</th>
                  </tr>
                </thead>
                <tbody>
                  {source.rows.map((row, i) => {
                    const st = statuses[i];
                    const meta = STATUS[st];
                    const tip = [meta.tip(), ...row.notes].filter(Boolean).join('\n');
                    const driverName = drivers.find((d) => d.id === row.connection.driver)?.name ?? row.connection.driver;
                    return (
                      <tr key={`${row.connection.id}-${i}`} className="h-(--h-row) border-b border-separator" data-status={done[i] ? 'done' : st}>
                        <td className="px-2">
                          <Checkbox
                            aria-label={row.connection.name}
                            checked={selected[i] || done[i]}
                            disabled={busy || !selectable[i]}
                            onChange={(v) => setSelected((s) => s.map((x, j) => (j === i ? v : x)))}
                          />
                        </td>
                        <td className="pr-2">
                          <span className="flex min-w-0 items-center gap-2">
                            <DriverBadge driverId={row.connection.driver || '?'} name={driverName} size="sm" />
                            <span className="truncate" title={row.connection.name}>
                              {row.connection.name}
                            </span>
                          </span>
                        </td>
                        <td className="truncate pr-2 text-fg-secondary">{driverName || '—'}</td>
                        <td className="truncate pr-2 text-sm text-fg-secondary" title={target(row)} data-selectable>
                          {target(row) || '—'}
                        </td>
                        <td className="pr-2">
                          <Tooltip content={tip || null}>
                            <span className="flex min-w-0 items-center gap-1.5">
                              {done[i] ? (
                                <Badge tone="success" hangul>
                                  <Check size={11} strokeWidth={2.25} /> {is('statusDone')}
                                </Badge>
                              ) : (
                                <Badge tone={meta.tone} hangul>
                                  {meta.label()}
                                </Badge>
                              )}
                              {row.notes.length > 0 && <span className="truncate text-sm text-fg-secondary">{row.notes.join(' · ')}</span>}
                            </span>
                          </Tooltip>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </>
        )}
      </div>
    </Dialog>
  );
}
