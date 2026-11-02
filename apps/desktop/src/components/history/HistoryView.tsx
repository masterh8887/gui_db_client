'use client';

// The history tab (DESIGN.md §5.F): connection `Select` (전체 + every saved connection; the
// filter runs server side through `history_list`), search over the loaded page, refresh, and a
// dense grid (다시 실행 first, 시작, 연결, 이름 with the SQL in its tooltip, 상태, 행, 크기,
// 시간, 파일 / error). A row click or `Enter` opens the entry in a new query tab; `Mod+Enter`
// runs it again; `Mod+C` copies its SQL.

import type { HistoryEntry } from 'ipc-types';
import { FolderOpen, RotateCcw, RotateCw } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { Button, EnvTag, IconButton, Notice, SearchField, Select, Spinner, StateChip, Tooltip } from '@/components/ui';
import { MetaGrid, type GridColumn } from '@/components/table/MetaGrid';
import { SEL_CHIP } from '@/components/table/table-ui';
import { connTag } from '@/lib/env';
import { baseName, formatBytes, formatDateTime, formatDuration, formatInt } from '@/lib/format';
import { useConnectionsStore } from '@/stores/connections';
import { selectHistoryPage, useHistoryStore } from '@/stores/history';
import { entryId, entryKeyDown, entryMenu, filterHistory, openEntry, openEntryFolder, runEntryAgain } from './history-ui';
import { hs } from './strings';

const ALL = '*';

/** Filter and search per history tab, kept for the session (the tab unmounts when inactive). */
const viewMemo = new Map<string, { filter: string; query: string }>();

/** For tests. */
export function __resetHistoryViewForTests(): void {
  viewMemo.clear();
}

/** SQL in a tooltip, cut so a huge statement does not cover the screen. */
function sqlTip(h: HistoryEntry): string {
  const sql = h.sql.length > 800 ? `${h.sql.slice(0, 800)}…` : h.sql;
  return h.sqlFile ? `${hs('sqlFile', { path: h.sqlFile })}\n\n${sql}` : sql;
}

export function HistoryView({ tabId, connId }: { tabId: string; connId: string }) {
  const [filter, setFilterState] = useState<string>(() => viewMemo.get(tabId)?.filter ?? connId);
  const [query, setQueryState] = useState<string>(() => viewMemo.get(tabId)?.query ?? '');
  const [activeId, setActiveId] = useState<string | null>(null);
  const key = filter === ALL ? null : filter;
  const page = useHistoryStore((s) => selectHistoryPage(s, key));
  const connections = useConnectionsStore((s) => s.connections);

  useEffect(() => {
    void useHistoryStore.getState().load(key);
  }, [key]);

  const remember = (patch: Partial<{ filter: string; query: string }>) => viewMemo.set(tabId, { filter, query, ...patch });
  const setFilter = (v: string) => {
    remember({ filter: v });
    setFilterState(v);
  };
  const setQuery = (v: string) => {
    remember({ query: v });
    setQueryState(v);
  };

  const conns = useMemo(() => new Map(connections.map((c) => [c.id, c])), [connections]);
  const connName = useMemo(() => (id: string) => conns.get(id)?.name ?? hs('unknownConn'), [conns]);
  const items = useMemo(() => filterHistory(page.entries, query, connName), [page.entries, query, connName]);
  const activeIndex = activeId === null ? -1 : items.findIndex((h) => entryId(h) === activeId);
  const reload = () => void useHistoryStore.getState().load(key);

  const columns: GridColumn<HistoryEntry>[] = [
    // First, so the action stays visible when long file names make the grid scroll sideways.
    {
      id: 'again',
      header: hs('col.again'),
      srOnlyHeader: true,
      width: 1,
      cell: (h) => (
        <Button
          size="sm"
          variant="ghost"
          tabIndex={-1}
          className="sel-muted -mx-1"
          icon={<RotateCcw size={13} strokeWidth={1.75} />}
          onClick={(e) => {
            e.stopPropagation();
            runEntryAgain(h);
          }}
          onDoubleClick={(e) => e.stopPropagation()}
        >
          {hs('runAgain')}
        </Button>
      ),
    },
    { id: 'started', header: hs('col.started'), className: 'tabular-nums sel-muted text-fg-secondary', cell: (h) => formatDateTime(h.startedAt) },
    {
      id: 'conn',
      header: hs('col.connection'),
      cell: (h) => {
        const c = conns.get(h.connId);
        return (
          <span className="flex items-center gap-1.5">
            <EnvTag tag={connTag(c)} variant="dot" />
            <span className={c ? undefined : 'sel-muted text-fg-secondary'}>{c?.name ?? hs('unknownConn')}</span>
          </span>
        );
      },
    },
    {
      id: 'name',
      header: hs('col.name'),
      cell: (h) => (
        <Tooltip content={sqlTip(h)}>
          <span className="block max-w-[280px] truncate font-medium">{h.label || hs('noName')}</span>
        </Tooltip>
      ),
    },
    { id: 'state', header: hs('col.state'), cell: (h) => <StateChip state={h.state} className={SEL_CHIP} /> },
    { id: 'rows', header: hs('col.rows'), align: 'right', className: 'tabular-nums', cell: (h) => formatInt(h.rows) },
    { id: 'bytes', header: hs('col.bytes'), align: 'right', className: 'tabular-nums sel-muted text-fg-secondary', cell: (h) => formatBytes(h.bytes) },
    { id: 'dur', header: hs('col.duration'), align: 'right', className: 'tabular-nums sel-muted text-fg-secondary', cell: (h) => formatDuration(h.durationMs) },
    {
      id: 'files',
      header: hs('col.files'),
      cell: (h) => {
        if (h.files.length === 0) {
          return h.error ? (
            <Tooltip content={h.error}>
              <span className="sel-muted block max-w-[360px] truncate text-sm text-danger">{h.error}</span>
            </Tooltip>
          ) : null;
        }
        const first = baseName(h.files[0]);
        return (
          <span className="flex items-center gap-1">
            <Tooltip content={h.files.join('\n')}>
              <span className="sel-muted block max-w-[320px] truncate text-sm text-fg-secondary">
                {first}
                {h.files.length > 1 && ` +${h.files.length - 1}`}
              </span>
            </Tooltip>
            <IconButton
              size="sm"
              tabIndex={-1}
              label={hs('openFolder')}
              className="sel-muted invisible group-hover/row:visible group-aria-selected/row:visible"
              onClick={(e) => {
                e.stopPropagation();
                openEntryFolder(h);
              }}
              onDoubleClick={(e) => e.stopPropagation()}
            >
              <FolderOpen size={14} strokeWidth={1.75} />
            </IconButton>
          </span>
        );
      },
    },
  ];

  return (
    <div className="flex h-full min-h-0 flex-col bg-window" data-tab={tabId}>
      <header className="flex h-9 shrink-0 items-center gap-2 border-b border-separator px-3">
        <h2 className="shrink-0 text-base font-semibold">{hs('title')}</h2>
        <div className="w-52 shrink-0">
          <Select
            size="sm"
            aria-label={hs('connection')}
            value={filter}
            onChange={setFilter}
            options={[{ value: ALL, label: hs('allConnections') }, ...connections.map((c) => ({ value: c.id, label: c.name }))]}
          />
        </div>
        <SearchField value={query} onChange={setQuery} placeholder={hs('search')} aria-label={hs('searchLabel')} className="w-60 max-w-[40%]" />
        {page.loading && <Spinner size={12} />}
        <span className="ml-auto shrink-0 text-sm tabular-nums text-fg-secondary">
          {query ? hs('countOf', { n: items.length, total: page.entries.length }) : hs('count', { n: page.entries.length })}
        </span>
        <IconButton size="sm" label={hs('refresh')} onClick={reload} disabled={page.loading}>
          <RotateCw size={14} strokeWidth={1.75} />
        </IconButton>
      </header>
      {page.error && (
        <div className="shrink-0 px-3 pt-2">
          <Notice tone="danger">
            <span className="flex items-center gap-2">
              <span className="min-w-0 flex-1">{page.error}</span>
              <Button size="sm" variant="secondary" onClick={reload}>
                {hs('retry')}
              </Button>
            </span>
          </Notice>
        </div>
      )}
      <div className="min-h-0 flex-1">
        <MetaGrid
          aria-label={hs('gridLabel')}
          columns={columns}
          rows={items}
          rowKey={entryId}
          activeIndex={activeIndex}
          onMove={(i) => setActiveId(items[i] ? entryId(items[i]) : null)}
          onOpen={(i) => items[i] && openEntry(items[i])}
          openOnClick
          rowContextItems={(h) => entryMenu(h)}
          onRowKeyDown={(e, i) => entryKeyDown(e, items[i])}
          empty={page.loading ? hs('loading') : query ? hs('noMatch') : hs('empty')}
        />
      </div>
    </div>
  );
}
