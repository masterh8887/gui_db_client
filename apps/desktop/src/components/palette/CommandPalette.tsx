'use client';

// Command palette / Open anything (DESIGN.md §5.H). `Mod+P` toggles it, `Mod+Shift+O` opens it on
// connections; `>` searches actions, `@` connections. Keyboard only: ↑↓ (or Ctrl+N / Ctrl+P)
// move, Enter opens (a table as a pinned tab, Mod+Enter in a new tab; a connection here, Mod+Enter
// in a new window, its "창 N" chips focus the windows that show it), Tab completes, Backspace
// on an empty input drops the scope chip, Esc clears, a second Esc closes. Actions run after the
// palette closed, on the next animation frame. No data rows anywhere: names and estimates only.

import * as RadixDialog from '@radix-ui/react-dialog';
import type { TableKind, TableRef } from 'ipc-types';
import { Database, History, Layers, Plug, Search, Table2, View, Zap } from 'lucide-react';
import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { getActions, runAction } from '@/lib/actions';
import { openQueryInConnection } from '@/lib/connection-actions';
import { inTauri } from '@/lib/ipc';
import { hasModKey, isMac } from '@/lib/platform';
import { cn, tableKey } from '@/lib/utils';
import { EnvTag, Highlight, KeyHint, RowEstimate, Tooltip } from '@/components/ui';
import { schemaKey, useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { HISTORY_ALL, useHistoryStore } from '@/stores/history';
import { useTabsStore, type WorkspaceTab } from '@/stores/tabs';
import { useUiStore, type PalettePick, type PaletteScope, type PaletteState } from '@/stores/ui';
import { otherWindowsWith, useWindowsStore, wins } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { buildGroups, parseQuery, type PaletteItem } from './sources';
import { ps } from './strings';

// ---- session memory -------------------------------------------------------------------------

const RECENT_ACTIONS_LIMIT = 8;
/** Actions run from the palette this session, most recent first. */
const recentActionIds: string[] = [];

function rememberAction(id: string): void {
  const i = recentActionIds.indexOf(id);
  if (i >= 0) recentActionIds.splice(i, 1);
  recentActionIds.unshift(id);
  recentActionIds.length = Math.min(recentActionIds.length, RECENT_ACTIONS_LIMIT);
}

/** Tests: forget the recent actions. */
export function __resetPaletteForTests(): void {
  recentActionIds.length = 0;
}

const nextFrame = (fn: () => void) => {
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(() => fn());
  else setTimeout(fn, 0);
};

// ---- component --------------------------------------------------------------------------------

export function CommandPalette() {
  const palette = useUiStore((s) => s.palette);
  if (!palette.open) return null;
  // A new `openPalette(...)` while open (menu, pick mode) starts over with its scope and query.
  return <PaletteDialog key={`${palette.scope}|${palette.pick?.tabId ?? ''}|${palette.query}`} init={palette} />;
}

function close(): void {
  useUiStore.getState().closePalette();
}

const SCOPE_CHIP: Record<Exclude<PaletteScope, 'all'>, () => string> = {
  tables: () => ps('scopeTables'),
  connections: () => ps('scopeConnections'),
  history: () => ps('scopeHistory'),
  actions: () => ps('scopeActions'),
};

function placeholder(scope: PaletteScope, pick: PalettePick | undefined, welcome: boolean): string {
  if (pick) return ps('placeholderPick');
  switch (scope) {
    case 'tables':
      return ps('placeholderTables');
    case 'connections':
      return ps('placeholderConnections');
    case 'history':
      return ps('placeholderHistory');
    case 'actions':
      return ps('placeholderActions');
    default:
      return welcome ? ps('placeholderWelcome') : ps('placeholderAll');
  }
}

function TableIcon({ kind }: { kind?: TableKind }) {
  const cls = 'sel-muted shrink-0 text-fg-secondary';
  if (kind === 'view') return <View size={14} strokeWidth={1.75} className={cls} />;
  if (kind === 'materialized_view') return <View size={14} strokeWidth={1.75} className="sel-muted shrink-0 text-accent" />;
  if (kind === 'partitioned') return <Layers size={14} strokeWidth={1.75} className={cls} />;
  if (kind === 'foreign') return <Plug size={14} strokeWidth={1.75} className={cls} />;
  return <Table2 size={14} strokeWidth={1.75} className={cls} />;
}

function ItemIcon({ item }: { item: PaletteItem }) {
  const cls = 'sel-muted shrink-0 text-fg-secondary';
  switch (item.kind) {
    case 'table':
      return <TableIcon kind={item.info?.kind} />;
    case 'connection':
      return <Database size={14} strokeWidth={1.75} className={cls} />;
    case 'history':
      return <History size={14} strokeWidth={1.75} className={cls} />;
    case 'action':
      return <Zap size={14} strokeWidth={1.75} className={cls} />;
  }
}

/**
 * "창 N" for each other window of this process showing the connection (docs/db-copy.md 1.1);
 * a click closes the palette and brings that window to the front. Mouse only: the keyboard stays
 * in the input.
 */
function WindowChips({ connId }: { connId: string }) {
  const windows = useWindowsStore(useShallow((s) => otherWindowsWith(s, connId)));
  return windows.map((w) => (
    <Tooltip key={w.label} content={wins('windowChipTip', { index: w.index, title: w.title })}>
      <button
        type="button"
        tabIndex={-1}
        data-window-chip={w.label}
        onMouseDown={(e) => {
          e.preventDefault();
          e.stopPropagation();
        }}
        onClick={(e) => {
          e.stopPropagation();
          close();
          void useWindowsStore.getState().focus(w.label);
        }}
        className="sel-muted inline-flex h-[18px] shrink-0 items-center rounded-sm bg-control px-1.5 text-sm font-medium leading-none text-fg-secondary tabular-nums hover:bg-hover hover:text-fg"
      >
        {wins('windowChip', { index: w.index })}
      </button>
    </Tooltip>
  ));
}

function Trailing({ item, selected, pickMode }: { item: PaletteItem; selected: boolean; pickMode: boolean }) {
  switch (item.kind) {
    case 'table':
      return (
        <>
          {(item.info?.estimatedRows !== undefined || item.exact) && (
            <RowEstimate rows={item.info?.estimatedRows} exact={item.exact} variant="compact" showLabel className="text-sm text-fg-secondary" />
          )}
          {selected && (
            <span className="flex items-center gap-1">
              <KeyHint combo="mod+enter" plain />
              <span className="sel-muted text-sm text-fg-secondary">{ps('newTab')}</span>
            </span>
          )}
        </>
      );
    case 'connection':
      return (
        <>
          {item.current ? (
            <span className="sel-muted text-sm text-fg-secondary">{ps('current')}</span>
          ) : (
            item.opened && <span className="sel-muted text-sm text-fg-secondary">{ps('opened')}</span>
          )}
          {!pickMode && <WindowChips connId={item.conn.id} />}
          {selected && !pickMode && (
            <span className="flex items-center gap-1">
              <KeyHint combo="mod+enter" plain />
              <span className="sel-muted text-sm text-fg-secondary">{ps('newWindow')}</span>
            </span>
          )}
          <EnvTag tag={item.conn.tag} variant={pickMode ? 'badge' : 'text'} />
        </>
      );
    case 'history':
      return null;
    case 'action':
      return selected && item.action.shortcut ? <KeyHint combo={item.action.shortcut} plain /> : null;
  }
}

function PaletteDialog({ init }: { init: PaletteState }) {
  const [scope, setScope] = useState<PaletteScope>(init.scope);
  const [pick, setPick] = useState<PalettePick | undefined>(init.pick);
  const [query, setQueryState] = useState(init.query);
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listId = useId();
  const optionId = (i: number) => `${listId}-o${i}`;

  const mode = useWorkspaceStore((s) => s.mode);
  const connId = useWorkspaceStore((s) => (s.mode === 'workspace' ? s.openConnId : null));
  const browse = useWorkspaceStore((s) => (connId ? s.scope[connId] : undefined));
  const opened = useWorkspaceStore((s) => s.opened);
  const database = browse?.database ?? null;
  const schema = browse?.schema ?? null;
  const tKey = connId && database && schema ? schemaKey(connId, database, schema) : null;
  const tables = useCatalogStore((s) => (tKey ? s.tables[tKey] : undefined));
  const tablesLoading = useCatalogStore((s) => (tKey ? !!s.loading[tKey] : false));
  const exactCounts = useCatalogStore((s) => s.exactCounts);
  const allTables = useCatalogStore((s) => s.tables);
  const connections = useConnectionsStore((s) => s.connections);
  const connPage = useHistoryStore((s) => (connId ? s.lists[connId] : undefined));
  const allPage = useHistoryStore((s) => s.lists[HISTORY_ALL]);
  const tabSet = useTabsStore((s) => (connId ? s.sets[connId] : undefined));
  const tabsById = useTabsStore((s) => s.tabs);
  const pickFrom = useTabsStore((s) => (pick ? (s.tabs[pick.tabId]?.connId ?? null) : null));
  // Enabled actions as of opening (the palette is short-lived).
  const [actions] = useState(() => getActions().filter((a) => !a.hidden && (a.enabled?.() ?? true)));
  const [recentActions] = useState(() => [...recentActionIds]);

  const effective = parseQuery(scope, query);
  const needsTables = !pick && connId !== null && (effective.scope === 'all' || effective.scope === 'tables');
  const needsHistory = !pick && connId !== null && (effective.scope === 'all' || effective.scope === 'history');

  // Load what the sources need: the current schema's tables, this connection's history.
  useEffect(() => {
    if (!needsTables || !connId || !database || !schema || tables) return;
    void useCatalogStore.getState().loadTables(connId, database, schema).catch(() => undefined);
  }, [needsTables, connId, database, schema, tables]);
  useEffect(() => {
    if (!needsHistory || !connId || connPage) return;
    void useHistoryStore.getState().load(connId);
  }, [needsHistory, connId, connPage]);

  const recentTables = useMemo(() => {
    if (!tabSet) return [];
    const out: TableRef[] = [];
    const seen = new Set<string>();
    const push = (tab: WorkspaceTab | undefined) => {
      if (tab?.kind !== 'table') return;
      const k = tableKey(tab.connId, tab.table);
      if (seen.has(k)) return;
      seen.add(k);
      out.push(tab.table);
    };
    push(tabSet.activeId ? tabsById[tabSet.activeId] : undefined);
    for (const id of [...tabSet.ids].reverse()) push(tabsById[id]);
    for (const c of [...tabSet.closed].reverse()) push(c.tab);
    return out;
  }, [tabSet, tabsById]);

  const history = useMemo(() => {
    const own = connPage?.entries ?? [];
    const ids = new Set(own.map((e) => String(e.id)));
    const others = (allPage?.entries ?? []).filter((e) => e.connId !== connId && !ids.has(String(e.id)));
    return [...own, ...others];
  }, [connPage, allPage, connId]);

  const groups = useMemo(
    () =>
      buildGroups({
        mode,
        connId,
        scope,
        query,
        pick: pick ? { fromConnId: pickFrom } : undefined,
        tables: tables ? { database, list: tables } : null,
        exactCounts,
        recentTables,
        findTable: (ref) => {
          if (!connId || !ref.database || !ref.schema) return undefined;
          return allTables[schemaKey(connId, ref.database, ref.schema)]?.find((t) => t.name === ref.name);
        },
        connections,
        opened,
        history,
        actions,
        recentActionIds: recentActions,
      }),
    [mode, connId, scope, query, pick, pickFrom, tables, database, exactCounts, recentTables, allTables, connections, opened, history, actions, recentActions],
  );
  const flat = useMemo(() => groups.flatMap((g) => g.items), [groups]);
  // Flat index of each group's first item.
  const starts = useMemo(() => {
    const out: number[] = [];
    let n = 0;
    for (const g of groups) {
      out.push(n);
      n += g.items.length;
    }
    return out;
  }, [groups]);
  const active = flat.length === 0 ? -1 : Math.min(sel, flat.length - 1);

  useEffect(() => {
    if (active < 0) return;
    document.getElementById(`${listId}-o${active}`)?.scrollIntoView?.({ block: 'nearest' });
  }, [active, listId]);

  const setQuery = (v: string) => {
    setQueryState(v);
    setSel(0);
  };

  const activate = (item: PaletteItem, newTab: boolean) => {
    close();
    switch (item.kind) {
      case 'table': {
        useWorkspaceStore.getState().select(item.connId, item.ref);
        useTabsStore.getState().openTable(item.connId, item.ref, newTab ? { newTab: true } : { preview: false });
        break;
      }
      case 'connection':
        if (pick) void openQueryInConnection(pick.tabId, item.conn.id);
        // Palette-local Mod+Enter (or Mod+click): 새 창에서 열기 (`window.openConnection`).
        else if (newTab) void useWindowsStore.getState().openNew(item.conn.id);
        else void useWorkspaceStore.getState().openWorkspace(item.conn.id);
        break;
      case 'history':
        void useWorkspaceStore.getState().openFromHistory(item.entry);
        break;
      case 'action': {
        const id = item.action.id;
        rememberAction(id);
        nextFrame(() => void runAction(id));
        break;
      }
    }
  };

  const chip = pick ? ps('scopePick') : scope !== 'all' ? SCOPE_CHIP[scope]() : null;

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return;
    const ctrlOnly = e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey;
    const key = e.key.toLowerCase();
    const move = (delta: number) => {
      e.preventDefault();
      if (flat.length === 0) return;
      setSel((active + delta + flat.length) % flat.length);
    };
    if (e.key === 'ArrowDown' || (ctrlOnly && (key === 'n' || e.code === 'KeyN'))) return move(1);
    if (e.key === 'ArrowUp' || (ctrlOnly && (key === 'p' || e.code === 'KeyP'))) return move(-1);
    if (e.key === 'PageDown') return move(Math.min(10, Math.max(1, flat.length - 1 - active)));
    if (e.key === 'PageUp') return move(-Math.min(10, Math.max(1, active)));
    if (e.key === 'Enter') {
      e.preventDefault();
      const item = active >= 0 ? flat[active] : undefined;
      if (item) activate(item, hasModKey(e));
      return;
    }
    if (e.key === 'Tab') {
      e.preventDefault();
      const item = active >= 0 ? flat[active] : undefined;
      if (item && !e.shiftKey) setQuery(`${effective.prefix}${item.title}`);
      return;
    }
    if (e.key === 'Backspace' && query === '' && chip) {
      e.preventDefault();
      setScope('all');
      setPick(undefined);
      setSel(0);
      return;
    }
    const mod = hasModKey(e) && !e.altKey;
    // The palette is a dialog, so the global shortcuts skip it: toggle and re-scope here. On macOS
    // inside Tauri the menu owns ⌘P and delivers it as `menu:action` instead.
    if (mod && !e.shiftKey && (key === 'p' || e.code === 'KeyP') && !(inTauri() && isMac())) {
      e.preventDefault();
      close();
      return;
    }
    if (mod && e.shiftKey && (key === 'o' || e.code === 'KeyO')) {
      e.preventDefault();
      setScope('connections');
      setPick(undefined);
      setQuery('');
    }
  };

  return (
    <RadixDialog.Root open onOpenChange={(o) => !o && close()}>
      <RadixDialog.Portal>
        <RadixDialog.Content
          data-roving-list
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            inputRef.current?.focus();
          }}
          onEscapeKeyDown={(e) => {
            // First Esc clears the text, the next one closes.
            if (query) {
              e.preventDefault();
              setQuery('');
            }
          }}
          className="animate-enter fixed top-[12vh] left-1/2 z-50 flex w-[600px] max-w-[calc(100vw-32px)] -translate-x-1/2 flex-col overflow-hidden rounded-lg bg-elevated text-fg shadow-dialog outline-none"
        >
          <RadixDialog.Title className="sr-only">{ps('title')}</RadixDialog.Title>
          <RadixDialog.Description className="sr-only">{ps('description')}</RadixDialog.Description>
          <div className="flex h-10 shrink-0 items-center gap-2 px-3">
            <Search size={16} strokeWidth={1.75} aria-hidden className="shrink-0 text-fg-tertiary" />
            {chip && (
              <span data-testid="palette-scope" className="inline-flex h-5 shrink-0 items-center rounded-sm bg-control px-1.5 text-sm font-medium text-fg-secondary">
                {chip}
              </span>
            )}
            <input
              ref={inputRef}
              type="text"
              role="combobox"
              aria-label={ps('title')}
              aria-expanded={flat.length > 0}
              aria-controls={listId}
              aria-autocomplete="list"
              aria-activedescendant={active >= 0 ? optionId(active) : undefined}
              placeholder={placeholder(scope, pick, mode !== 'workspace')}
              value={query}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={onKeyDown}
              className="min-w-0 flex-1 bg-transparent text-lg text-fg outline-none placeholder:text-fg-tertiary"
            />
          </div>
          <div
            id={listId}
            role="listbox"
            aria-label={ps('results')}
            className={cn('list-scroll max-h-[352px] border-t border-separator py-1', flat.length === 0 && 'hidden')}
          >
            {groups.map((g, gi) => (
              <div key={g.id} role="group" aria-labelledby={`${listId}-g-${g.id}`}>
                <div id={`${listId}-g-${g.id}`} className="flex h-(--h-row-sm) items-center px-3 text-sm font-semibold text-fg-secondary">
                  {g.title}
                </div>
                {g.items.map((item, k) => {
                  const i = starts[gi] + k;
                  const selected = i === active;
                  return (
                    <div
                      key={item.id}
                      id={optionId(i)}
                      role="option"
                      aria-selected={selected}
                      data-kind={item.kind}
                      onMouseMove={() => {
                        if (i !== active) setSel(i);
                      }}
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={(e) => activate(item, hasModKey(e))}
                      className="mx-1 flex h-7 min-w-0 items-center gap-2 rounded-md px-2 text-base"
                    >
                      <ItemIcon item={item} />
                      <Highlight text={item.title} indices={item.indices} className="min-w-0 shrink truncate" />
                      {item.secondary && <span className="sel-muted min-w-0 shrink-[2] truncate text-sm text-fg-secondary">{item.secondary}</span>}
                      <span className="ml-auto flex shrink-0 items-center gap-2 pl-2">
                        <Trailing item={item} selected={selected} pickMode={!!pick} />
                      </span>
                    </div>
                  );
                })}
              </div>
            ))}
          </div>
          {flat.length === 0 && (
            <div className="flex h-9 items-center gap-2 border-t border-separator px-4 text-base text-fg-secondary">
              {needsTables && tablesLoading ? (
                ps('loadingTables')
              ) : (
                <>
                  <span>{ps('noResults')}</span>
                  {!pick && scope === 'all' && <span className="text-sm text-fg-secondary">{ps('noResultsHint')}</span>}
                </>
              )}
            </div>
          )}
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}
