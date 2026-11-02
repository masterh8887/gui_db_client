'use client';

// Welcome / connection manager (DESIGN.md §5.A): a compact centred surface with the app mark and
// the main actions on the left, and the searchable, groupable connection list on the right.
// Keyboard: ↑↓ Home End select, Enter / double-click open the workspace, Mod+Enter opens it in a
// new window, Delete or Mod+Backspace delete (confirm), Mod+F or any printable key focus the
// search, Esc clears it. Connections open in other windows of this process carry "창 N" chips
// (click: focus that window) and are listed first under "열린 연결" (docs/db-copy.md 1.1).

import { ArrowDownUp, ChevronDown, Copy, Database, FileInput, Plus } from 'lucide-react';
import { Fragment, useEffect, useMemo, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { appInfo } from '@/lib/commands';
import { deleteConnection, newConnection, rescanDrivers, startImportDbeaver } from '@/lib/connection-actions';
import { connTag } from '@/lib/env';
import { useInTauri } from '@/lib/ipc';
import { hasModKey, useIsMac } from '@/lib/platform';
import { useGlobalShortcuts, type ShortcutBinding } from '@/lib/shortcuts';
import {
  Badge,
  Button,
  DropdownMenu,
  EmptyState,
  EnvTag,
  IconButton,
  KeyHint,
  SearchField,
  Spinner,
  useListNavigation,
  type MenuItem,
} from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore, type WelcomeGroupBy } from '@/stores/ui';
import { connIdsInOtherWindows, otherWindowsWith, useWindowsStore } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { bestMatch, flattenGroups, groupConnections, matchConnections, withOpenGroup } from './connection-list';
import { ConnectionRow, connectionMenuItems, openInNewWindow } from './ConnectionRow';
import { InstancesChip } from './InstancesChip';
import { ws } from './strings';

/** Nothing (or only the Welcome surface itself) has focus: list keys may act globally. */
function isIdleTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return true;
  return target === document.body || target === document.documentElement || target.hasAttribute('data-welcome-surface');
}

// Elements are found through data attributes (not refs) so the key handlers stay plain functions.
const welcomeSearch = () => document.querySelector<HTMLInputElement>('[data-welcome-surface] input[type="search"]');
const welcomeList = () => document.querySelector<HTMLElement>('[data-welcome-list]');

export function WelcomeScreen({ headerEnd }: { headerEnd?: ReactNode }) {
  const connections = useConnectionsStore((s) => s.connections);
  const drivers = useConnectionsStore((s) => s.drivers);
  const loading = useConnectionsStore((s) => s.loading);
  const loaded = useConnectionsStore((s) => s.loaded);
  const selectedId = useConnectionsStore((s) => s.selectedId);
  const select = useConnectionsStore((s) => s.select);
  const status = useConnectionsStore((s) => s.status);
  const errors = useConnectionsStore((s) => s.errors);
  const groupBy = useUiStore((s) => s.welcomeGroupBy);
  const setGroupBy = useUiStore((s) => s.setWelcomeGroupBy);
  const opened = useWorkspaceStore((s) => s.opened);
  const tabCounts = useTabsStore(useShallow((s) => Object.fromEntries(opened.map((id) => [id, s.sets[id]?.ids.length ?? 0]))));
  const windowsState = useWindowsStore(useShallow((s) => ({ self: s.self, windows: s.windows })));
  const elsewhere = useMemo(() => connIdsInOtherWindows(windowsState), [windowsState]);
  const mac = useIsMac();

  const [query, setQuery] = useState('');

  const searching = query.trim() !== '';
  const openIds = useMemo(() => new Set([...opened, ...elsewhere]), [opened, elsewhere]);
  const matches = useMemo(() => matchConnections(connections, drivers, query), [connections, drivers, query]);
  const groups = useMemo(
    () => withOpenGroup(groupConnections(matches, groupBy, drivers, searching), groupBy, searching, openIds),
    [matches, groupBy, drivers, searching, openIds],
  );
  const rows = useMemo(() => flattenGroups(groups), [groups]);
  const indexOf = useMemo(() => new Map(rows.map((c, i) => [c.id, i])), [rows]);
  const activeIndex = selectedId ? (indexOf.get(selectedId) ?? -1) : -1;

  const openAt = (i: number) => {
    const c = rows[i];
    if (c) void useWorkspaceStore.getState().openWorkspace(c.id);
  };
  const removeById = async (id: string) => {
    const i = indexOf.get(id) ?? -1;
    const next = i >= 0 ? (rows[i + 1] ?? rows[i - 1]) : undefined;
    if (await deleteConnection(id)) select(next?.id ?? null);
  };
  const nav = useListNavigation({
    count: rows.length,
    activeIndex,
    onMove: (i) => select(rows[i]?.id ?? null),
    onOpen: openAt,
    onDelete: (i) => {
      const c = rows[i];
      if (c) void removeById(c.id);
    },
  });

  const onQueryChange = (v: string) => {
    setQuery(v);
    if (!v.trim()) return;
    // The best hit is selected, so Enter opens it.
    const best = bestMatch(matchConnections(connections, drivers, v));
    if (best) select(best.conn.id);
  };

  // Mod+Enter: the selected connection in a new window (Welcome-local, docs/db-copy.md 1.1).
  const openSelectedInNewWindow = (e: { preventDefault(): void }): boolean => {
    const c = activeIndex >= 0 ? rows[activeIndex] : undefined;
    if (!c) return false;
    e.preventDefault();
    openInNewWindow(c.id);
    return true;
  };
  const isModEnter = (e: { key: string; metaKey: boolean; ctrlKey: boolean }) => e.key === 'Enter' && hasModKey(e);

  // ↑ ↓ and Enter work from the search field too (the field keeps focus).
  const onSearchKey = (e: ReactKeyboardEvent<HTMLInputElement>) => {
    if (isModEnter(e) && openSelectedInNewWindow(e)) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter') nav.onKeyDown(e);
  };
  const listProps = {
    ...nav.listProps,
    onKeyDown: (e: ReactKeyboardEvent) => {
      if (isModEnter(e) && openSelectedInNewWindow(e)) return;
      nav.listProps.onKeyDown(e);
    },
  };

  // Local keys while Welcome is mounted (KEYMAP's welcome rows are labels only). The list
  // handles its own keys when focused; these cover "nothing focused".
  const listKey = (key: string, extra: Partial<ShortcutBinding> = {}): ShortcutBinding => ({
    key,
    code: key === 'Backspace' || key === 'Delete' || key === 'Enter' ? key : undefined,
    ...extra,
    when: (e) => isIdleTarget(e.target),
    handler: (e) => {
      welcomeList()?.focus();
      nav.onKeyDown(e as unknown as ReactKeyboardEvent);
    },
  });
  useGlobalShortcuts([
    {
      key: 'f',
      code: 'KeyF',
      mod: true,
      inInputs: true,
      handler: () => {
        const input = welcomeSearch();
        input?.focus();
        input?.select();
      },
    },
    listKey('ArrowDown'),
    listKey('ArrowUp'),
    listKey('Home'),
    listKey('End'),
    listKey('Enter'),
    {
      key: 'Enter',
      code: 'Enter',
      mod: true,
      when: (e) => isIdleTarget(e.target),
      handler: (e) => void openSelectedInNewWindow(e),
    },
    listKey('Delete'),
    listKey('Backspace', { mod: true }),
  ]);

  // A printable key typed while nothing (or the list) is focused moves focus to the search on
  // keydown, without preventDefault, so the character lands in the field. Skipped while an IME
  // composes (known limit: the first Hangul jamo can be lost; Mod+F always works).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing || e.key === 'Process' || e.keyCode === 229) return;
      if (e.key.length !== 1 || e.key === ' ' || e.ctrlKey || e.metaKey || e.altKey) return;
      const t = e.target;
      if (!isIdleTarget(t) && !(t instanceof Element && t.hasAttribute('data-welcome-list'))) return;
      welcomeSearch()?.focus();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const sortItems: MenuItem[] = [
    { id: 'view', label: ws('view'), kind: 'label' },
    ...(
      [
        ['none', ws('byName')],
        ['tag', ws('byTag')],
        ['driver', ws('byDriver')],
      ] as [WelcomeGroupBy, string][]
    ).map(([g, label]) => ({ id: `group-${g}`, label, kind: 'radio' as const, checked: groupBy === g, onSelect: () => setGroupBy(g) })),
    { id: 'rescan', label: ws('rescan'), separatorBefore: true, onSelect: () => void rescanDrivers() },
  ];

  const driversKnown = drivers.length > 0;
  const renderRow = (c: (typeof rows)[number], nameIndices: readonly number[]) => {
    const i = indexOf.get(c.id) ?? -1;
    const isOpen = opened.includes(c.id);
    return (
      <ConnectionRow
        key={c.id}
        conn={c}
        driver={drivers.find((d) => d.id === c.driver)}
        driversKnown={driversKnown}
        nameIndices={nameIndices}
        tabCount={isOpen ? (tabCounts[c.id] ?? 0) : null}
        otherWindows={otherWindowsWith(windowsState, c.id)}
        status={status[c.id]}
        error={errors[c.id]}
        itemProps={nav.getItemProps(i)}
        onContextMenu={() => select(c.id)}
        menuItems={connectionMenuItems(c, { isOpen, mac, onDelete: () => void removeById(c.id) })}
      />
    );
  };

  let body: ReactNode;
  if (connections.length === 0) {
    body =
      loading && !loaded ? (
        <div className="flex flex-1 items-center justify-center gap-2 text-fg-secondary">
          <Spinner size={14} /> {ws('loading')}
        </div>
      ) : (
        <EmptyState
          icon={<Database size={28} strokeWidth={1.5} />}
          title={ws('empty')}
          hint={ws('emptyHint')}
          action={
            <>
              <Button variant="primary" icon={<Plus size={14} />} onClick={newConnection}>
                {ws('newConnection')}
              </Button>
              <Button variant="secondary" icon={<FileInput size={14} />} onClick={startImportDbeaver}>
                {ws('importDbeaver')}
              </Button>
            </>
          }
        />
      );
  } else if (rows.length === 0) {
    body = <EmptyState title={ws('noMatch')} hint={ws('noMatchHint')} />;
  } else {
    body = (
      <div {...listProps} data-welcome-list="" aria-label={ws('listLabel')} className="list-scroll min-h-0 flex-1 px-1.5 pt-1 pb-2 outline-none">
        {groups.map((g) =>
          g.label === null ? (
            <Fragment key={g.key}>{g.items.map((m) => renderRow(m.conn, m.nameIndices))}</Fragment>
          ) : (
            <div key={g.key} role="group" aria-label={g.label}>
              <div
                role="presentation"
                className="sticky top-0 z-10 flex h-(--h-row-sm) items-center gap-1.5 bg-window px-2 text-sm font-semibold text-fg-secondary"
              >
                {g.key.startsWith('tag:') && <EnvTag tag={g.tag} variant="dot" />}
                <span className="truncate">{g.label}</span>
                <span className="text-xs font-normal text-fg-tertiary tabular-nums">{g.items.length}</span>
              </div>
              {g.items.map((m) => renderRow(m.conn, m.nameIndices))}
            </div>
          ),
        )}
      </div>
    );
  }

  return (
    <div
      data-welcome-surface=""
      className="flex h-[min(540px,calc(100%-64px))] w-[min(820px,calc(100%-64px))] overflow-hidden rounded-lg bg-window text-fg shadow-dialog"
    >
      <WelcomeSidePane />
      <section className="flex min-w-0 flex-1 flex-col">
        <div className="flex h-11 shrink-0 items-center gap-1 border-b border-separator pr-2 pl-3">
          <SearchField
            size="md"
            className="mr-1 flex-1"
            value={query}
            onChange={onQueryChange}
            placeholder={ws('searchPlaceholder')}
            shortcut="mod+f"
            onKeyDown={onSearchKey}
            aria-label={ws('searchLabel')}
          />
          <InstancesChip className="mr-1" />
          {headerEnd}
          <DropdownMenu items={sortItems}>
            <Button variant="ghost" aria-label={ws('sort')}>
              <ArrowDownUp size={14} strokeWidth={1.75} className="text-fg-secondary" />
              <span>{ws('sort')}</span>
              <ChevronDown size={12} strokeWidth={1.75} />
            </Button>
          </DropdownMenu>
          <IconButton label={ws('newConnection')} onClick={newConnection}>
            <Plus size={16} strokeWidth={1.75} />
          </IconButton>
        </div>
        {body}
      </section>
    </div>
  );
}

/** Left pane: mark, name, version (+ `mock`), the main actions, open workspaces, footer links. */
function WelcomeSidePane() {
  const tauri = useInTauri();
  const [version, setVersion] = useState<string | null>(null);
  const opened = useWorkspaceStore((s) => s.opened);
  const connections = useConnectionsStore((s) => s.connections);
  const tabCounts = useTabsStore(useShallow((s) => Object.fromEntries(opened.map((id) => [id, s.sets[id]?.ids.length ?? 0]))));
  const openConns = opened.map((id) => connections.find((c) => c.id === id)).filter((c) => c !== undefined).slice(0, 5);

  useEffect(() => {
    let alive = true;
    appInfo()
      .then((info) => {
        if (alive) setVersion(info.version);
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, []);

  return (
    <aside className="flex w-[260px] shrink-0 flex-col bg-sidebar">
      <div className="flex flex-col items-center px-5 pt-10 pb-6">
        <div className="flex h-14 w-14 items-center justify-center rounded-[14px] bg-accent-soft text-accent">
          <Database size={28} strokeWidth={1.75} />
        </div>
        <h1 className="mt-3 text-xl font-semibold">BulkDB</h1>
        <div className="mt-0.5 flex h-[18px] items-center gap-1.5 text-xs text-fg-tertiary tabular-nums">
          {version && <span>v{version}</span>}
          {!tauri && (
            <Badge tone="warning" title={ws('mockTip')}>
              {ws('mock')}
            </Badge>
          )}
        </div>
      </div>
      <nav className="flex flex-col gap-0.5 px-3">
        <PaneAction icon={<Plus size={14} strokeWidth={1.75} />} label={ws('newConnection')} shortcut="mod+n" onClick={newConnection} />
        <PaneAction icon={<FileInput size={14} strokeWidth={1.75} />} label={ws('importDbeaver')} onClick={startImportDbeaver} />
        <PaneAction icon={<Copy size={14} strokeWidth={1.75} />} label={ws('copyDatabase')} onClick={() => useCopyStore.getState().open()} />
      </nav>
      {openConns.length > 0 && (
        <div className="mt-5 flex min-h-0 flex-col px-3">
          <div className="flex h-(--h-row-sm) items-center px-2.5 text-sm font-semibold text-fg-secondary">{ws('openWorkspaces')}</div>
          {openConns.map((c) => (
            <button
              key={c.id}
              type="button"
              onClick={() => void useWorkspaceStore.getState().openWorkspace(c.id)}
              className="flex h-(--h-row) items-center gap-2 rounded-md px-2.5 text-left text-base hover:bg-hover active:bg-pressed focus-visible:outline-2 focus-visible:outline-focus"
            >
              <EnvTag tag={connTag(c)} variant="dot" />
              <span className="min-w-0 flex-1 truncate">{c.name}</span>
              <span className="shrink-0 text-sm text-fg-secondary tabular-nums">{ws('tabs', { n: tabCounts[c.id] ?? 0 })}</span>
            </button>
          ))}
        </div>
      )}
      <div className="mt-auto flex items-center justify-center gap-1.5 px-4 pb-4 text-sm text-fg-secondary">
        <button type="button" className="rounded-sm hover:text-fg focus-visible:outline-2 focus-visible:outline-focus" onClick={() => useUiStore.getState().setSettingsOpen(true)}>
          {ws('settings')}
        </button>
        <span aria-hidden className="text-fg-tertiary">
          ·
        </span>
        <button type="button" className="rounded-sm hover:text-fg focus-visible:outline-2 focus-visible:outline-focus" onClick={() => useUiStore.getState().setAboutOpen(true)}>
          {ws('about')}
        </button>
      </div>
    </aside>
  );
}

function PaneAction({ icon, label, shortcut, onClick }: { icon: ReactNode; label: string; shortcut?: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex h-8 items-center gap-2 rounded-md px-2.5 text-left text-base text-fg transition-colors duration-[80ms] hover:bg-hover active:bg-pressed focus-visible:outline-2 focus-visible:outline-focus"
    >
      <span className="text-fg-secondary">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {shortcut && <KeyHint combo={shortcut} plain />}
    </button>
  );
}
