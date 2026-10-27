'use client';

import type { HistoryEntry } from 'ipc-types';
import { RefreshCw, RotateCcw } from 'lucide-react';
import { useEffect } from 'react';
import { baseName, formatBytes, formatDateTime, formatDuration, formatInt } from '@/lib/format';
import { stateLabel, t } from '@/lib/i18n';
import { STATE_TONE } from '@/lib/job-state';
import { Badge, Button, IconButton, Notice, Select, Spinner, Table, type Column } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';


export function HistoryTab() {
  const entries = useHistoryStore((s) => s.entries);
  const loading = useHistoryStore((s) => s.loading);
  const error = useHistoryStore((s) => s.error);
  const connId = useHistoryStore((s) => s.connId);
  const load = useHistoryStore((s) => s.load);
  const setConnId = useHistoryStore((s) => s.setConnId);
  const connections = useConnectionsStore((s) => s.connections);
  const loadFromHistory = useExtractStore((s) => s.loadFromHistory);

  useEffect(() => {
    void load();
  }, [load]);

  const connName = (id: string) => connections.find((c) => c.id === id)?.name ?? id;
  /** Load the entry into the Extract tab and start the normal run flow (preflight first). */
  const runAgain = (h: HistoryEntry) => {
    loadFromHistory(h);
    void useExtractStore.getState().run();
  };

  const columns: Column<HistoryEntry>[] = [
    // First, so the action stays visible when long file names make the table scroll sideways.
    {
      id: 'again',
      header: '',
      width: 110,
      cell: (h) => (
        <Button
          size="sm"
          variant="outline"
          icon={<RotateCcw size={12} />}
          onClick={(e) => {
            e.stopPropagation();
            runAgain(h);
          }}
        >
          {t('history.runAgain')}
        </Button>
      ),
    },
    { id: 'started', header: t('history.started'), cell: (h) => <span className="tabular-nums">{formatDateTime(h.startedAt)}</span>, width: 120 },
    { id: 'conn', header: t('history.connection'), cell: (h) => connName(h.connId) },
    { id: 'label', header: t('history.label'), cell: (h) => <span title={h.sql}>{h.label}</span> },
    { id: 'state', header: t('history.state'), cell: (h) => <Badge tone={STATE_TONE[h.state]}>{stateLabel(h.state)}</Badge> },
    { id: 'rows', header: t('history.rows'), cell: (h) => formatInt(h.rows), align: 'right', className: 'tabular-nums' },
    { id: 'bytes', header: t('history.bytes'), cell: (h) => formatBytes(h.bytes), align: 'right', className: 'tabular-nums' },
    { id: 'dur', header: t('history.duration'), cell: (h) => formatDuration(h.durationMs), align: 'right', className: 'tabular-nums' },
    {
      id: 'files',
      header: t('history.files'),
      cell: (h) => (
        <span className="text-xs text-muted-foreground" title={h.files.join('\n')}>
          {h.files.length === 0 ? (h.error ? <span className="text-danger">{h.error}</span> : '') : h.files.length === 1 ? baseName(h.files[0]) : `${baseName(h.files[0])} +${h.files.length - 1}`}
        </span>
      ),
    },
  ];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-2 border-b border-border px-2">
        <span className="text-xs font-semibold text-muted-foreground">{t('history.title')}</span>
        <div className="w-48">
          <Select
            size="sm"
            aria-label={t('history.connection')}
            value={connId ?? 'all'}
            onChange={(v) => void setConnId(v === 'all' ? null : v)}
            options={[{ value: 'all', label: t('history.allConnections') }, ...connections.map((c) => ({ value: c.id, label: c.name }))]}
          />
        </div>
        {loading && <Spinner size={12} />}
        <span className="truncate text-xs text-muted-foreground">{t('history.loadHint')}</span>
        <IconButton label={t('history.refresh')} size="sm" className="ml-auto" onClick={() => void load()}>
          <RefreshCw size={12} />
        </IconButton>
      </div>
      {error && (
        <div className="p-2">
          <Notice tone="danger">{error}</Notice>
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-auto">
        <Table columns={columns} rows={entries} rowKey={(h) => String(h.id)} empty={t('history.empty')} onRowClick={loadFromHistory} />
      </div>
    </div>
  );
}
