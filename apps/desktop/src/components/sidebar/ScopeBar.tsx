'use client';

// Sidebar bottom bar (DESIGN.md §5.C "Sidebar"): database and schema switchers with searchable
// popovers, and the connection refresh. Above it, while the browsed database is not the one
// queries run in, a persistent "탐색 전용" row.

import type { DatabaseInfo, SchemaInfo } from 'ipc-types';
import { ChevronDown, Database, FolderTree, RotateCw } from 'lucide-react';
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from 'react';
import { IconButton, Popover, SearchField, Spinner, StatusBar, Tooltip, useListNavigation } from '@/components/ui';
import { formatBytes, formatInt, num } from '@/lib/format';
import { fuzzyMatch } from '@/lib/fuzzy';
import { comboLabel } from '@/lib/keymap';
import { cn } from '@/lib/utils';
import { dbKey, useCatalogStore } from '@/stores/catalog';
import { useWorkspaceStore } from '@/stores/workspace';
import { ts } from './strings';

export type ScopePickerKind = 'database' | 'schema';

interface PickerItem {
  value: string;
  label: string;
  /** Right-aligned secondary text (size, table count). */
  detail?: string;
  detailLabel?: string;
  /** `(현재)` / `(실행)` marks after the name. */
  marks?: string;
  disabled?: boolean;
  /** Tooltip (encoding / collation, inaccessible). */
  title?: string;
}

/** Searchable list in a popover: ↑↓ Home End move (from the search field too), Enter picks. */
function ScopePicker({
  open,
  onOpenChange,
  anchor,
  title,
  searchLabel,
  items,
  value,
  loading,
  error,
  onRetry,
  onPick,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  anchor: RefObject<HTMLElement | null>;
  title: string;
  searchLabel: string;
  items: PickerItem[];
  value: string | null;
  loading: boolean;
  error?: string;
  onRetry: () => void;
  onPick: (value: string) => void;
}) {
  return (
    <Popover open={open} onOpenChange={onOpenChange} anchor={anchor} side="top" align="start" width={264} aria-label={title}>
      <PickerBody
        title={title}
        searchLabel={searchLabel}
        items={items}
        value={value}
        loading={loading}
        error={error}
        onRetry={onRetry}
        onPick={(v) => {
          onPick(v);
          onOpenChange(false);
        }}
      />
    </Popover>
  );
}

function PickerBody({
  title,
  searchLabel,
  items,
  value,
  loading,
  error,
  onRetry,
  onPick,
}: {
  title: string;
  searchLabel: string;
  items: PickerItem[];
  value: string | null;
  loading: boolean;
  error?: string;
  onRetry: () => void;
  onPick: (value: string) => void;
}) {
  const [query, setQuery] = useState('');
  const filtered = useMemo(() => {
    const q = query.trim();
    if (!q) return items.map((item) => ({ item, indices: [] as number[] }));
    const out: { item: PickerItem; indices: number[] }[] = [];
    for (const item of items) {
      const m = fuzzyMatch(q, item.label);
      if (m) out.push({ item, indices: m.indices });
    }
    return out;
  }, [items, query]);
  // The highlighted row, by value: starts on the current scope; while filtering, a highlight that
  // no longer matches falls back to the first enabled match.
  const [activeValue, setActiveValue] = useState<string | null>(value);
  let active = filtered.findIndex((f) => f.item.value === activeValue);
  if (active < 0 && query.trim()) active = filtered.findIndex((f) => !f.item.disabled);
  const setActive = (i: number) => setActiveValue(filtered[i]?.item.value ?? null);

  const pick = (i: number) => {
    const f = filtered[i];
    if (f && !f.item.disabled) onPick(f.item.value);
  };
  const nav = useListNavigation({ count: filtered.length, activeIndex: active, onMove: setActive, onOpen: pick });
  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter') nav.onKeyDown(e);
  };

  // Focus stays in the search field, so the field is the combobox that names the highlighted row
  // (`aria-activedescendant`); the list itself is never focused. `SearchField` only forwards
  // `aria-label`, so the combobox attributes are set on its input here.
  const listId = useId();
  const searchRef = useRef<HTMLInputElement>(null);
  const activeId = active >= 0 && active < filtered.length ? nav.itemId(active) : undefined;
  useLayoutEffect(() => {
    const el = searchRef.current;
    if (!el) return;
    el.setAttribute('role', 'combobox');
    el.setAttribute('aria-controls', listId);
    el.setAttribute('aria-expanded', 'true');
    el.setAttribute('aria-autocomplete', 'list');
    if (activeId) el.setAttribute('aria-activedescendant', activeId);
    else el.removeAttribute('aria-activedescendant');
  }, [listId, activeId]);

  return (
    <div className="flex flex-col">
      <div className="px-3 pt-2.5 pb-1.5 text-sm font-semibold text-fg-secondary">{title}</div>
      <div className="px-2 pb-1.5">
        <SearchField
          value={query}
          onChange={setQuery}
          aria-label={searchLabel}
          placeholder={ts('search')}
          autoFocus
          inputRef={searchRef}
          onKeyDown={onSearchKey}
        />
      </div>
      <div
        {...nav.listProps}
        id={listId}
        tabIndex={-1}
        // The focused search field carries the active descendant, not this list.
        aria-activedescendant={undefined}
        // Clicks keep the focus in the search field (type, ↑↓, Enter keep working).
        onMouseDown={(e) => e.preventDefault()}
        aria-label={title}
        className="list-scroll max-h-[280px] overflow-y-auto border-t border-separator p-1 outline-none"
      >
        {loading && items.length === 0 ? (
          <div className="flex h-(--h-row) items-center gap-2 px-2 text-sm text-fg-secondary">
            <Spinner size={12} />
            {ts('loading')}
          </div>
        ) : error && items.length === 0 ? (
          <div className="flex flex-col items-start gap-1 px-2 py-1.5 text-sm">
            <span data-selectable className="text-danger">
              {error}
            </span>
            <button type="button" className="text-accent hover:underline" onClick={onRetry}>
              {ts('retry')}
            </button>
          </div>
        ) : filtered.length === 0 ? (
          <div className="flex h-(--h-row) items-center px-2 text-sm text-fg-secondary">{ts('noResults')}</div>
        ) : (
          filtered.map(({ item }, i) => {
            const row = (
              <div
                key={item.value}
                id={nav.itemId(i)}
                role="option"
                aria-selected={i === active}
                aria-disabled={item.disabled || undefined}
                aria-current={item.value === value || undefined}
                onClick={() => {
                  setActive(i);
                  pick(i);
                }}
                className={cn(
                  'flex h-(--h-row) select-none items-center gap-1.5 rounded-md px-2',
                  i === active ? 'bg-selected-focus text-on-accent' : 'hover:bg-hover',
                  item.disabled && 'opacity-60',
                )}
              >
                <span className={cn('min-w-0 truncate', item.value === value && 'font-medium', item.disabled && 'line-through')}>
                  {item.label}
                </span>
                {item.marks && (
                  <span className={cn('shrink-0 text-sm', i === active ? 'text-on-accent-muted' : 'text-fg-secondary')}>{item.marks}</span>
                )}
                {item.detail && (
                  <span
                    aria-label={item.detailLabel}
                    className={cn('ml-auto shrink-0 text-xs tabular-nums', i === active ? 'text-on-accent-muted' : 'text-fg-secondary')}
                  >
                    {item.detail}
                  </span>
                )}
              </div>
            );
            return item.title ? (
              <Tooltip key={item.value} content={item.title} side="right">
                {row}
              </Tooltip>
            ) : (
              row
            );
          })
        )}
      </div>
    </div>
  );
}

function databaseItems(list: readonly DatabaseInfo[], execDb: string | null): PickerItem[] {
  return list.map((d) => {
    const marks = [d.isCurrent ? ts('current') : '', d.name === execDb ? ts('exec') : ''].filter(Boolean).join(' ');
    const tip = !d.accessible
      ? ts('inaccessible')
      : d.encoding
        ? ts('encoding', { encoding: d.encoding, collation: d.collation ?? '—' })
        : undefined;
    return {
      value: d.name,
      label: d.name,
      marks: d.accessible ? marks || undefined : [marks, ts('inaccessible')].filter(Boolean).join(' '),
      detail: num(d.sizeBytes) !== undefined ? formatBytes(d.sizeBytes, 0) : undefined,
      disabled: !d.accessible,
      title: tip,
    };
  });
}

function schemaItems(list: readonly SchemaInfo[]): PickerItem[] {
  return list.map((s) => {
    const count = num(s.tableCount);
    return {
      value: s.name,
      label: s.name,
      detail: count !== undefined ? formatInt(count) : undefined,
      detailLabel: count !== undefined ? ts('tableCount', { count: formatInt(count) }) : undefined,
    };
  });
}

/** One switcher button of the bar (ghost, 22px, icon + truncated name + chevron). */
function ScopeButton({
  anchorRef,
  icon,
  text,
  label,
  tip,
  open,
  disabled,
  onClick,
  className,
}: {
  anchorRef: RefObject<HTMLButtonElement | null>;
  icon: ReactNode;
  text: string;
  label: string;
  tip: string;
  open: boolean;
  disabled?: boolean;
  onClick: () => void;
  className?: string;
}) {
  return (
    <Tooltip content={tip}>
      <button
        ref={anchorRef}
        type="button"
        aria-label={label}
        aria-haspopup="dialog"
        aria-expanded={open}
        disabled={disabled}
        onClick={onClick}
        className={cn(
          'flex h-(--h-control-sm) min-w-0 items-center gap-1 rounded-md px-1.5 text-sm text-fg-secondary outline-none hover:bg-hover hover:text-fg focus-visible:outline-2 focus-visible:outline-focus disabled:opacity-40',
          open && 'bg-pressed text-fg',
          className,
        )}
      >
        <span className="inline-flex shrink-0 [&_svg]:size-3.5">{icon}</span>
        <span className="min-w-0 truncate">{text}</span>
        <ChevronDown size={12} strokeWidth={1.75} className="shrink-0 text-fg-tertiary" aria-hidden />
      </button>
    </Tooltip>
  );
}

export function BrowseOnlyRow({ browse, execDb }: { browse: string; execDb: string }) {
  return (
    <Tooltip content={ts('browseOnlyTip', { browse, db: execDb })}>
      <div role="note" className="flex h-(--h-row-sm) shrink-0 items-center bg-warning-soft px-2.5 text-sm text-warning">
        <span className="min-w-0 truncate">{ts('browseOnly', { db: execDb })}</span>
      </div>
    </Tooltip>
  );
}

export function ScopeBar({
  connId,
  database,
  schema,
  execDb,
  picker,
  onPickerChange,
  refreshing,
  onRefresh,
}: {
  connId: string;
  database: string | null;
  schema: string | null;
  execDb: string | null;
  picker: ScopePickerKind | null;
  onPickerChange: (p: ScopePickerKind | null) => void;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const dbRef = useRef<HTMLButtonElement>(null);
  const schemaRef = useRef<HTMLButtonElement>(null);
  const databases = useCatalogStore((s) => s.databases[connId]);
  const dbLoading = useCatalogStore((s) => !!s.loading[connId]);
  const dbError = useCatalogStore((s) => s.errors[connId]);
  const sKey = database ? dbKey(connId, database) : '';
  const schemas = useCatalogStore((s) => (sKey ? s.schemas[sKey] : undefined));
  const schemasLoading = useCatalogStore((s) => (sKey ? !!s.loading[sKey] : false));
  const schemasError = useCatalogStore((s) => (sKey ? s.errors[sKey] : undefined));

  const dbItems = useMemo(() => databaseItems(databases ?? [], execDb), [databases, execDb]);
  const scItems = useMemo(() => schemaItems(schemas ?? []), [schemas]);

  // Opening the schema list loads it when nothing is cached yet.
  useEffect(() => {
    if (picker !== 'schema' || !database) return;
    const s = useCatalogStore.getState();
    const key = dbKey(connId, database);
    if (s.schemas[key] || s.loading[key] || s.errors[key]) return;
    void s.loadSchemas(connId, database).catch(() => undefined);
  }, [picker, connId, database]);

  const setScope = useWorkspaceStore((s) => s.setScope);
  const toggle = (p: ScopePickerKind) => onPickerChange(picker === p ? null : p);
  const close = (o: boolean) => {
    if (!o) onPickerChange(null);
  };

  return (
    <StatusBar className="gap-0.5 px-1">
      <ScopeButton
        anchorRef={dbRef}
        icon={<Database strokeWidth={1.75} />}
        text={database ?? ts('noDatabase')}
        label={ts('databaseButton', { name: database ?? '—' })}
        tip={`${ts('pickDatabase')} (${comboLabel('mod+k')})`}
        open={picker === 'database'}
        onClick={() => toggle('database')}
        className="max-w-[55%] shrink"
      />
      <ScopeButton
        anchorRef={schemaRef}
        icon={<FolderTree strokeWidth={1.75} />}
        text={schema ?? ts('noSchema')}
        label={ts('schemaButton', { name: schema ?? '—' })}
        tip={ts('pickSchema')}
        open={picker === 'schema'}
        disabled={!database}
        onClick={() => toggle('schema')}
        className="shrink"
      />
      <IconButton label={ts('refresh')} size="sm" className="ml-auto" disabled={refreshing} onClick={onRefresh}>
        {refreshing ? <Spinner size={12} /> : <RotateCw size={14} strokeWidth={1.75} />}
      </IconButton>

      <ScopePicker
        open={picker === 'database'}
        onOpenChange={close}
        anchor={dbRef}
        title={ts('pickDatabase')}
        searchLabel={ts('searchDatabases')}
        items={dbItems}
        value={database}
        loading={dbLoading}
        error={dbError}
        onRetry={() => void useCatalogStore.getState().loadDatabases(connId, true).catch(() => undefined)}
        onPick={(name) => setScope(connId, { database: name })}
      />
      <ScopePicker
        open={picker === 'schema'}
        onOpenChange={close}
        anchor={schemaRef}
        title={ts('pickSchema')}
        searchLabel={ts('searchSchemas')}
        items={scItems}
        value={schema}
        loading={schemasLoading}
        error={schemasError}
        onRetry={() => {
          if (database) void useCatalogStore.getState().loadSchemas(connId, database, true).catch(() => undefined);
        }}
        onPick={(name) => setScope(connId, { schema: name })}
      />
    </StatusBar>
  );
}
