'use client';

// Left sidebar, 항목 segment (DESIGN.md §5.C "Sidebar"): search, the browsed schema's objects in
// sections by kind (sticky headers, 추정 captions), roving keyboard selection, row menus, and the
// database / schema switcher with the connection refresh at the bottom. Only metadata is shown;
// row counts are estimates unless an exact count exists. Only the browsed schema's objects load.

import type { TableInfo, TableKind } from 'ipc-types';
import { CircleAlert, KeyRound, Layers, Plug, Table2, View } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import {
  Button,
  EmptyState,
  Highlight,
  SearchField,
  SidebarItem,
  SidebarSection,
  Spinner,
  Tooltip,
  useListNavigation,
} from '@/components/ui';
import { hasModKey } from '@/lib/platform';
import { openExtractionQuery, useExecDatabase } from '@/lib/table-actions';
import { tableKey } from '@/lib/utils';
import { dbKey, schemaKey, useCatalogStore } from '@/stores/catalog';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore, type SidebarRequest } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import {
  buildItemModel,
  collapseKey,
  refOf,
  rowAriaLabel,
  rowTooltip,
  rowTrailing,
  sameRef,
  sectionTitle,
} from './model';
import { BrowseOnlyRow, ScopeBar, type ScopePickerKind } from './ScopeBar';
import { ts } from './strings';
import { openStructure, tableMenuItems } from './tableMenu';

const ICON_PROPS = { size: 14, strokeWidth: 1.75 } as const;

function kindIcon(kind: TableKind): ReactNode {
  switch (kind) {
    case 'partitioned':
      return <Layers {...ICON_PROPS} />;
    case 'view':
      return <View {...ICON_PROPS} />;
    case 'materialized_view':
      // Accent while unselected; inherits the row colour once selected (the focused selection is accent).
      return <View {...ICON_PROPS} className="text-accent group-aria-selected/item:text-inherit" />;
    case 'foreign':
      return <Plug {...ICON_PROPS} />;
    default:
      return <Table2 {...ICON_PROPS} />;
  }
}

function LoadingRow() {
  return (
    <div className="flex h-(--h-row) items-center gap-2 px-3 text-sm text-fg-secondary">
      <Spinner size={12} />
      {ts('loading')}
    </div>
  );
}

function ErrorBlock({ error, onRetry }: { error: string; onRetry: () => void }) {
  return (
    <div role="alert" className="flex flex-col items-start gap-1.5 px-3 py-2 text-sm">
      <span data-selectable className="flex min-w-0 items-start gap-1.5 text-danger">
        <CircleAlert size={14} strokeWidth={1.75} className="mt-px shrink-0" aria-hidden />
        <span className="min-w-0 break-words">{error}</span>
      </span>
      <Button size="sm" variant="secondary" onClick={onRetry}>
        {ts('retry')}
      </Button>
    </div>
  );
}

function Quiet({ children }: { children: ReactNode }) {
  return <div className="px-3 py-2 text-sm text-fg-secondary">{children}</div>;
}

export function Sidebar({ connId }: { connId: string }) {
  const [query, setQuery] = useState('');
  const [picker, setPicker] = useState<ScopePickerKind | null>(null);
  // Connections with a refresh in flight: the spinner and `aria-busy` stay with the workspace
  // that started the refresh, not with whichever one is shown when it finishes.
  const [refreshingConns, setRefreshingConns] = useState<ReadonlySet<string>>(() => new Set());
  const refreshing = refreshingConns.has(connId);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  // Another workspace: its own filter, no picker left open.
  const [shownConn, setShownConn] = useState(connId);
  if (shownConn !== connId) {
    setShownConn(connId);
    setQuery('');
    setPicker(null);
  }

  const scope = useWorkspaceStore((s) => s.scope[connId]);
  const database = scope?.database ?? null;
  const schema = scope?.schema ?? null;
  const selection = useWorkspaceStore((s) => s.selection[connId] ?? null);
  const execDb = useExecDatabase(connId);
  const needsPassword = useConnectionsStore((s) => needsSessionPassword(s, connId));

  const databases = useCatalogStore((s) => s.databases[connId]);
  const dbError = useCatalogStore((s) => s.errors[connId]);
  const sKey = database ? dbKey(connId, database) : '';
  const schemas = useCatalogStore((s) => (sKey ? s.schemas[sKey] : undefined));
  const schemasError = useCatalogStore((s) => (sKey ? s.errors[sKey] : undefined));
  const tKey = database && schema ? schemaKey(connId, database, schema) : '';
  const tables = useCatalogStore((s) => (tKey ? s.tables[tKey] : undefined));
  const tablesError = useCatalogStore((s) => (tKey ? s.errors[tKey] : undefined));
  const exactCounts = useCatalogStore((s) => s.exactCounts);
  const counting = useCatalogStore((s) => s.counting);
  const collapsed = useUiStore((s) => s.collapsedSections);
  const toggleSection = useUiStore((s) => s.toggleSection);
  // The table shown in this workspace's preview tab (italic row).
  const previewKey = useTabsStore((s) => {
    for (const id of s.sets[connId]?.ids ?? []) {
      const tab = s.tabs[id];
      if (tab?.kind === 'table' && tab.preview) return tableKey(connId, tab.table);
    }
    return null;
  });

  // No scope or no databases yet (the sidebar mounted before `openWorkspace` resolved them, or the
  // cache was dropped): load the databases and resolve the scope. The catalog store dedupes loads.
  const hasDatabases = !!databases;
  useEffect(() => {
    if (needsPassword || dbError || (scope && hasDatabases)) return;
    void useCatalogStore
      .getState()
      .loadDatabases(connId)
      .then(() => useWorkspaceStore.getState().ensureScope(connId))
      .catch(() => undefined);
  }, [connId, needsPassword, scope, hasDatabases, dbError]);

  // A database without cached schemas (after a refresh, say): load them for the empty state.
  const hasSchemas = !!schemas;
  useEffect(() => {
    if (!database || hasSchemas || schemasError || needsPassword) return;
    const s = useCatalogStore.getState();
    if (s.loading[dbKey(connId, database)]) return;
    void s.loadSchemas(connId, database).catch(() => undefined);
  }, [connId, database, hasSchemas, schemasError, needsPassword]);

  // Lazy load: only the browsed schema's objects, once (a refresh clears them and reloads).
  const hasTables = !!tables;
  useEffect(() => {
    if (!database || !schema || hasTables || tablesError || needsPassword) return;
    const s = useCatalogStore.getState();
    if (s.loading[schemaKey(connId, database, schema)]) return;
    void s.loadTables(connId, database, schema).catch(() => undefined);
  }, [connId, database, schema, hasTables, tablesError, needsPassword]);

  // `ui.sidebarRequest` (Mod+Shift+F, Mod+K, palette): open the picker while rendering, focus
  // in an effect, then clear the request.
  const request = useUiStore((s) => s.sidebarRequest);
  const [seenRequest, setSeenRequest] = useState<SidebarRequest | null>(null);
  if (request && request !== seenRequest) {
    setSeenRequest(request);
    if (request.kind !== 'focusSearch') setPicker(request.kind);
  }
  useEffect(() => {
    if (!request) return;
    if (request.kind === 'focusSearch') {
      searchRef.current?.focus();
      searchRef.current?.select();
    }
    useUiStore.getState().clearSidebarRequest();
  }, [request]);

  const refresh = useCallback(async () => {
    setRefreshingConns((s) => new Set(s).add(connId));
    try {
      // Invalidate the backend cache, drop ours, reload the databases, then re-check the scope;
      // the effects above reload the schemas and objects that are on screen.
      await useCatalogStore.getState().refresh(connId);
      await useWorkspaceStore.getState().ensureScope(connId);
    } catch {
      // Shown from `catalog.errors`.
    } finally {
      setRefreshingConns((s) => {
        const next = new Set(s);
        next.delete(connId);
        return next;
      });
    }
  }, [connId]);

  const exactOf = useCallback(
    (t: Pick<TableInfo, 'schema' | 'name'>) => exactCounts[tableKey(connId, refOf(database, t))],
    [exactCounts, connId, database],
  );
  const model = useMemo(
    () => buildItemModel(tables ?? [], query, collapsed, (t) => !!exactOf(t)),
    [tables, query, collapsed, exactOf],
  );
  const { visible } = model;
  const activeIndex = selection ? visible.findIndex((t) => sameRef(selection, refOf(database, t))) : -1;

  const select = (t: TableInfo) => useWorkspaceStore.getState().select(connId, refOf(database, t));
  const openPinned = (t: TableInfo) => {
    select(t);
    openStructure(connId, refOf(database, t));
  };
  const openPreview = (t: TableInfo) => {
    select(t);
    useTabsStore.getState().openTable(connId, refOf(database, t), { preview: true });
  };
  const makeQuery = (t: TableInfo) => {
    select(t);
    void openExtractionQuery(connId, refOf(database, t));
  };

  const nav = useListNavigation({
    count: visible.length,
    activeIndex,
    onMove: (i) => select(visible[i]),
    onOpen: (i) => openPinned(visible[i]),
  });

  const onListKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    // Keys on a section header button or a row's `⋯` button are theirs.
    if (e.target !== e.currentTarget) return;
    if (e.key === 'Enter' && hasModKey(e)) {
      const t = visible[activeIndex];
      if (!t) return;
      e.preventDefault();
      makeQuery(t);
      return;
    }
    if (e.key === 'Escape' && query) {
      e.preventDefault();
      setQuery('');
      return;
    }
    // Type to search: a printable key moves focus to the search field (the key lands there).
    if (e.key.length === 1 && e.key !== ' ' && !e.ctrlKey && !e.metaKey && !e.altKey && !e.nativeEvent.isComposing) {
      searchRef.current?.focus();
      return;
    }
    nav.onKeyDown(e);
  };

  const onSearchKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return;
    const target = visible[activeIndex] ?? visible[0];
    if (e.key === 'ArrowDown') {
      if (visible.length === 0) return;
      e.preventDefault();
      if (activeIndex < 0) select(visible[0]);
      listRef.current?.focus();
    } else if (e.key === 'Enter' && target) {
      e.preventDefault();
      if (hasModKey(e)) makeQuery(target);
      else openPinned(target);
    }
  };

  // ---- body: password / errors / loading / empty / list --------------------------------
  let body: ReactNode;
  const retryDatabases = () =>
    void useCatalogStore
      .getState()
      .loadDatabases(connId, true)
      .then(() => useWorkspaceStore.getState().ensureScope(connId))
      .catch(() => undefined);
  if (needsPassword) {
    body = (
      <EmptyState
        compact
        icon={<KeyRound size={20} strokeWidth={1.75} />}
        title={ts('passwordNeeded')}
        hint={ts('passwordHint')}
        action={
          <Button size="sm" variant="secondary" onClick={() => useUiStore.getState().askDbPassword(connId, 'browse')}>
            {ts('enterPassword')}
          </Button>
        }
      />
    );
  } else if (!databases) {
    body = dbError ? <ErrorBlock error={dbError} onRetry={retryDatabases} /> : <LoadingRow />;
  } else if (databases.length === 0 || scope?.database === null) {
    body = <Quiet>{ts('noDatabases')}</Quiet>;
  } else if (!database) {
    body = <LoadingRow />;
  } else if (!schemas) {
    body = schemasError ? (
      <ErrorBlock
        error={schemasError}
        onRetry={() =>
          void useCatalogStore
            .getState()
            .loadSchemas(connId, database, true)
            .then(() => useWorkspaceStore.getState().ensureScope(connId))
            .catch(() => undefined)
        }
      />
    ) : (
      <LoadingRow />
    );
  } else if (schemas.length === 0) {
    body = <Quiet>{ts('noSchemas')}</Quiet>;
  } else if (!schema) {
    body = <LoadingRow />;
  } else if (!tables) {
    body = tablesError ? (
      <ErrorBlock
        error={tablesError}
        onRetry={() => void useCatalogStore.getState().loadTables(connId, database, schema, true).catch(() => undefined)}
      />
    ) : (
      <LoadingRow />
    );
  } else if (tables.length === 0) {
    body = <Quiet>{ts('noTables')}</Quiet>;
  } else if (model.matchCount === 0) {
    body = <Quiet>{ts('noMatches', { query: query.trim() })}</Quiet>;
  }

  const visibleIndex = new Map(visible.map((t, i) => [t, i] as const));
  const list =
    body === undefined ? (
      model.sections.map((sec) => (
        <SidebarSection
          key={sec.key}
          id={sec.key}
          title={sectionTitle(sec.key)}
          count={sec.rows.length}
          caption={
            sec.hasEstimate ? (
              <Tooltip content={ts('estimatedCaptionTip')}>
                <span>{ts('estimated')}</span>
              </Tooltip>
            ) : undefined
          }
          collapsed={sec.collapsed}
          onToggle={() => toggleSection(collapseKey(sec.key))}
        >
          {sec.rows.map(({ table, indices }) => {
            const i = visibleIndex.get(table) ?? -1;
            const ref = refOf(database, table);
            const key = tableKey(connId, ref);
            const exact = exactCounts[key];
            return (
              <SidebarItem
                key={`${table.kind}:${table.name}`}
                id={nav.itemId(i)}
                icon={kindIcon(table.kind)}
                label={indices.length > 0 ? <Highlight text={table.name} indices={indices} /> : table.name}
                trailing={rowTrailing(table, exact)}
                selected={i >= 0 && i === activeIndex}
                preview={key === previewKey}
                title={rowTooltip(table, exact)}
                aria-label={rowAriaLabel(table, exact)}
                onClick={() => openPreview(table)}
                onDoubleClick={() => openPinned(table)}
                contextItems={tableMenuItems({ connId, ref, counting: !!counting[key], onRefresh: () => void refresh() })}
              />
            );
          })}
        </SidebarSection>
      ))
    ) : (
      body
    );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="shrink-0 px-2 pt-1 pb-1.5">
        <SearchField
          value={query}
          onChange={setQuery}
          inputRef={searchRef}
          placeholder={ts('search')}
          shortcut="mod+shift+f"
          aria-label={ts('searchLabel')}
          onKeyDown={onSearchKeyDown}
        />
      </div>
      <div
        ref={listRef}
        {...nav.listProps}
        onKeyDown={onListKeyDown}
        aria-label={ts('items')}
        aria-busy={refreshing || undefined}
        className="list-scroll min-h-0 flex-1 overflow-y-auto px-1.5 pb-1.5 outline-none"
      >
        {databases && dbError && <ErrorBlock error={dbError} onRetry={() => void refresh()} />}
        {list}
      </div>
      {database && execDb && database !== execDb && <BrowseOnlyRow browse={database} execDb={execDb} />}
      <ScopeBar
        connId={connId}
        database={database}
        schema={schema}
        execDb={execDb}
        picker={picker}
        onPickerChange={setPicker}
        refreshing={refreshing}
        onRefresh={() => void refresh()}
      />
    </div>
  );
}
