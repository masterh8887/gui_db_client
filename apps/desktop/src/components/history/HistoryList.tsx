'use client';

// Sidebar 기록 (DESIGN.md §5.F): search over the loaded page, `[이 연결 | 전체]` (server-side
// filter through `history_list`), 44px two-line rows (state icon + name / time · rows · size;
// in 전체, other connections' rows also name their connection). Click or `Enter` opens the entry
// in a new query tab (another connection's entry switches workspaces); hover 다시 실행; menu.

import type { HistoryEntry } from 'ipc-types';
import { History, MoreHorizontal, RotateCcw } from 'lucide-react';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Button, ContextMenu, DropdownMenu, EmptyState, IconButton, SearchField, SegmentedControl, Spinner, StatusBar, useListNavigation } from '@/components/ui';
import { formatBytes, formatCompact, num } from '@/lib/format';
import { cn } from '@/lib/utils';
import { useConnectionsStore } from '@/stores/connections';
import { selectHistoryPage, useHistoryStore } from '@/stores/history';
import { useTabsStore } from '@/stores/tabs';
import { shortDateTime } from '@/components/table/table-ui';
import { entryId, entryKeyDown, entryMenu, filterHistory, HistoryStateIcon, openEntry, runEntryAgain } from './history-ui';
import { hs } from './strings';

type Scope = 'this' | 'all';

/** The scope per connection, kept for the session (the list remounts with the 항목 | 기록 switch). */
const scopeMemo = new Map<string, Scope>();

/** For tests. */
export function __resetHistoryListForTests(): void {
  scopeMemo.clear();
}

function secondLine(h: HistoryEntry, connName: string | null): string {
  const parts: string[] = [];
  if (connName) parts.push(connName);
  parts.push(shortDateTime(h.startedAt));
  const rows = num(h.rows);
  if (rows !== undefined) parts.push(hs('rows', { n: formatCompact(rows) }));
  if (num(h.bytes) !== undefined) parts.push(formatBytes(h.bytes));
  return parts.join(' · ');
}

export function HistoryList({ connId }: { connId: string }) {
  const [scope, setScopeState] = useState<Scope>(() => scopeMemo.get(connId) ?? 'this');
  const [query, setQuery] = useState('');
  const [activeId, setActiveId] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const key = scope === 'this' ? connId : null;
  const page = useHistoryStore((s) => selectHistoryPage(s, key));
  const connections = useConnectionsStore((s) => s.connections);

  useEffect(() => {
    void useHistoryStore.getState().load(key);
  }, [key]);

  const setScope = (s: Scope) => {
    scopeMemo.set(connId, s);
    setScopeState(s);
  };

  const connName = useMemo(() => {
    const names = new Map(connections.map((c) => [c.id, c.name]));
    return (id: string) => names.get(id) ?? hs('unknownConn');
  }, [connections]);
  const items = useMemo(() => filterHistory(page.entries, query, connName), [page.entries, query, connName]);
  const activeIndex = activeId === null ? -1 : items.findIndex((h) => entryId(h) === activeId);

  const nav = useListNavigation({
    count: items.length,
    activeIndex,
    onMove: (i) => setActiveId(items[i] ? entryId(items[i]) : null),
    onOpen: (i) => items[i] && openEntry(items[i]),
  });
  const reload = () => void useHistoryStore.getState().load(key);
  const fullHistory = () => useTabsStore.getState().openHistory(connId);

  let content;
  if (page.loading && page.entries.length === 0) {
    content = (
      <div role="status" className="flex h-(--h-row) items-center gap-2 px-3 text-sm text-fg-secondary">
        <Spinner size={12} /> {hs('loading')}
      </div>
    );
  } else if (page.error && page.entries.length === 0) {
    content = (
      <div role="alert" className="flex flex-col items-start gap-2 px-3 py-2 text-sm">
        <span className="text-danger" data-selectable>
          {page.error}
        </span>
        <Button size="sm" variant="secondary" onClick={reload}>
          {hs('retry')}
        </Button>
      </div>
    );
  } else if (items.length === 0) {
    content = query ? <EmptyState compact title={hs('noMatch')} /> : <EmptyState compact icon={<History size={20} strokeWidth={1.75} />} title={hs('empty')} hint={hs('emptyHint')} />;
  } else {
    content = (
      <div
        ref={listRef}
        {...nav.listProps}
        aria-label={hs('listLabel')}
        onKeyDown={(e) => {
          entryKeyDown(e, items[activeIndex]);
          if (!e.defaultPrevented) nav.onKeyDown(e);
        }}
        className="list-scroll min-h-0 flex-1 px-1 py-0.5 outline-none"
      >
        {items.map((h, i) => {
          const props = nav.getItemProps(i);
          const other = scope === 'all' && h.connId !== connId;
          return (
            <ContextMenu key={entryId(h)} items={entryMenu(h, { fullHistoryConnId: connId })}>
              <div
                {...props}
                onClick={(e) => {
                  props.onClick();
                  // The second click of a double-click must not open a second tab.
                  if (e.detail <= 1) openEntry(h);
                }}
                onDoubleClick={undefined}
                onContextMenu={() => setActiveId(entryId(h))}
                className="group relative flex h-(--h-row-2) items-center gap-2 rounded-md pr-1 pl-2 aria-selected:bg-selected [&:not([aria-selected=true]):hover]:bg-hover"
              >
                <HistoryStateIcon state={h.state} />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-base font-medium">{h.label || hs('noName')}</div>
                  <div className="sel-muted truncate text-sm text-fg-secondary">{secondLine(h, other ? connName(h.connId) : null)}</div>
                </div>
                <div
                  className={cn('shrink-0 items-center', i === activeIndex ? 'flex' : 'hidden group-hover:flex')}
                  onClick={(e) => e.stopPropagation()}
                  onDoubleClick={(e) => e.stopPropagation()}
                >
                  <IconButton size="sm" tabIndex={-1} label={hs('runAgain')} className="sel-muted" onClick={() => runEntryAgain(h)}>
                    <RotateCcw size={14} strokeWidth={1.75} />
                  </IconButton>
                  <DropdownMenu items={entryMenu(h, { fullHistoryConnId: connId })}>
                    <IconButton size="sm" tabIndex={-1} label={hs('more')} noTooltip className="sel-muted">
                      <MoreHorizontal size={14} strokeWidth={1.75} />
                    </IconButton>
                  </DropdownMenu>
                </div>
              </div>
            </ContextMenu>
          );
        })}
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 flex-col gap-1.5 px-2 pt-1 pb-1.5">
        <SearchField
          value={query}
          onChange={setQuery}
          placeholder={hs('search')}
          aria-label={hs('searchLabel')}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown' && items.length > 0) {
              e.preventDefault();
              if (activeIndex < 0) setActiveId(entryId(items[0]));
              listRef.current?.focus();
            }
          }}
        />
        <SegmentedControl<Scope>
          aria-label={hs('scope')}
          value={scope}
          onChange={setScope}
          fill
          options={[
            { value: 'this', label: hs('scopeThis') },
            { value: 'all', label: hs('scopeAll') },
          ]}
        />
      </div>
      <div className="flex min-h-0 flex-1 flex-col">{content}</div>
      <StatusBar>
        <span className="flex min-w-0 items-center gap-1.5 text-fg-secondary">
          {page.loading && page.entries.length > 0 && <Spinner size={10} />}
          <span className="truncate tabular-nums">
            {query ? hs('countOf', { n: items.length, total: page.entries.length }) : hs('count', { n: page.entries.length })}
          </span>
        </span>
        <Button size="sm" variant="ghost" className="ml-auto" onClick={fullHistory}>
          {hs('fullHistory')}
        </Button>
      </StatusBar>
    </div>
  );
}
