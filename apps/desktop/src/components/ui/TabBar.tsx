'use client';

import { Plus, X } from 'lucide-react';
import { useEffect, useRef, type DragEvent, type KeyboardEvent, type ReactNode } from 'react';
import { isMac } from '@/lib/platform';
import { cn } from '@/lib/utils';
import { ContextMenu, type MenuItem } from './ContextMenu';
import { IconButton } from './IconButton';
import { Spinner } from './Spinner';
import { us } from './strings';
import { Tooltip } from './Tooltip';

const DRAG_TYPE = 'application/x-bulkdb-tab';

export interface TabBarProps {
  children: ReactNode;
  onNewTab: () => void;
  /** Right-click menu of the `+` button. */
  newTabContextItems?: MenuItem[];
  /** Double-click on the empty strip. */
  onEmptyDoubleClick?: () => void;
  /** Drag to reorder (indices among the tabs). */
  onReorder?: (from: number, to: number) => void;
  'aria-label': string;
  /** Shown instead of the tabs when there are none. */
  empty?: ReactNode;
}

function tabElements(list: HTMLElement | null): HTMLElement[] {
  return list ? Array.from(list.querySelectorAll<HTMLElement>('[role="tab"]')) : [];
}

/**
 * Workspace tab strip (DESIGN.md §5.C "Tab bar"): 30px, equal-width tabs (min 120px, scroll
 * only below that), a plain `+` button. Arrow keys / Home / End move between tabs and
 * activate them.
 */
export function TabBar({ children, onNewTab, newTabContextItems, onEmptyDoubleClick, onReorder, empty, ...aria }: TabBarProps) {
  const listRef = useRef<HTMLDivElement>(null);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if ((e.target as HTMLElement).getAttribute('role') !== 'tab') return;
    const tabs = tabElements(listRef.current);
    const i = tabs.indexOf(e.target as HTMLElement);
    let next = -1;
    if (e.key === 'ArrowRight') next = (i + 1) % tabs.length;
    else if (e.key === 'ArrowLeft') next = (i - 1 + tabs.length) % tabs.length;
    else if (e.key === 'Home') next = 0;
    else if (e.key === 'End') next = tabs.length - 1;
    if (next < 0 || !tabs[next]) return;
    e.preventDefault();
    tabs[next].focus();
    tabs[next].click();
  };

  const onDragOver = (e: DragEvent<HTMLDivElement>) => {
    if (onReorder && e.dataTransfer.types.includes(DRAG_TYPE)) {
      e.preventDefault();
      e.dataTransfer.dropEffect = 'move';
    }
  };

  const onDrop = (e: DragEvent<HTMLDivElement>) => {
    if (!onReorder) return;
    const id = e.dataTransfer.getData(DRAG_TYPE);
    if (!id) return;
    e.preventDefault();
    const tabs = tabElements(listRef.current);
    const from = tabs.findIndex((t) => t.dataset.tabId === id);
    const overTab = (e.target as HTMLElement).closest<HTMLElement>('[role="tab"]');
    const to = overTab ? tabs.indexOf(overTab) : tabs.length - 1;
    if (from >= 0 && to >= 0 && from !== to) onReorder(from, to);
  };

  const plus = (
    <IconButton label={us('newTab')} size="sm" onClick={onNewTab} className="mx-1 self-center">
      <Plus size={14} strokeWidth={1.75} />
    </IconButton>
  );

  return (
    // The bottom hairline is an inset shadow, so the active tab (bg-window) paints over it and
    // joins the content below, as in TablePlus.
    <div className="flex h-(--h-tabbar) shrink-0 items-stretch bg-tabbar shadow-[inset_0_-1px_0_var(--separator)]">
      <div
        ref={listRef}
        role="tablist"
        aria-label={aria['aria-label']}
        onKeyDown={onKeyDown}
        onDragOver={onDragOver}
        onDrop={onDrop}
        onDoubleClick={(e) => {
          if (e.target === e.currentTarget) onEmptyDoubleClick?.();
        }}
        className="flex min-w-0 flex-1 items-stretch overflow-x-auto overflow-y-hidden [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
      >
        {children}
        {empty}
      </div>
      {newTabContextItems && newTabContextItems.length > 0 ? (
        <ContextMenu items={newTabContextItems} className="flex items-stretch">
          {plus}
        </ContextMenu>
      ) : (
        plus
      )}
    </div>
  );
}

function refocus(input: HTMLInputElement | null) {
  if (!input || !input.isConnected || document.activeElement === input) return;
  input.focus();
  input.select();
}

/** Focus steals within this long after the rename input mounts are undone (see RenameInput). */
const RENAME_FOCUS_GRACE_MS = 300;

/**
 * Inline rename: Enter / blur commit once, Esc cancels (consumed). An unchanged title cancels too,
 * so a default title never turns into a custom one.
 */
function RenameInput({
  title,
  onRename,
  onCancel,
}: {
  title: string;
  onRename?: (title: string | null) => void;
  onCancel?: () => void;
}) {
  const done = useRef(false);
  const ref = useRef<HTMLInputElement>(null);
  // The rename starts from the tab's context menu, whose focus scope hands focus back to the tab
  // (or the editor) in a timeout after the menu closed, i.e. after this input mounted. A blur that
  // soon, before the user did anything, is that hand-back: take focus again instead of committing.
  const mountedAt = useRef<number | null>(null); // null: still mounting (autoFocus may blur already)
  const interacted = useRef(false);
  useEffect(() => {
    mountedAt.current = performance.now();
    const t = window.setTimeout(() => {
      if (!done.current) refocus(ref.current);
    }, 0);
    return () => window.clearTimeout(t);
  }, []);
  const finish = (value: string | null) => {
    if (done.current) return;
    done.current = true;
    if (value === null) onCancel?.();
    else if (value.trim() === title) onCancel?.();
    else onRename?.(value.trim() || null);
  };
  return (
    <input
      ref={ref}
      autoFocus
      defaultValue={title}
      aria-label={title}
      data-esc-local
      spellCheck={false}
      onFocus={(e) => e.currentTarget.select()}
      onPointerDown={() => {
        interacted.current = true;
      }}
      onChange={() => {
        interacted.current = true;
      }}
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        interacted.current = true;
        if (e.key === 'Enter') {
          e.preventDefault();
          finish(e.currentTarget.value);
        } else if (e.key === 'Escape') {
          e.preventDefault();
          e.stopPropagation();
          finish(null);
        }
      }}
      onBlur={(e) => {
        const age = mountedAt.current === null ? 0 : performance.now() - mountedAt.current;
        if (!interacted.current && age < RENAME_FOCUS_GRACE_MS) {
          const input = e.currentTarget;
          window.setTimeout(() => {
            if (!done.current) refocus(input);
          }, 0);
          return;
        }
        finish(e.currentTarget.value);
      }}
      className="input-focus h-5 w-full min-w-0 rounded-[4px] border border-border-input bg-input px-1 text-center text-sm text-fg"
    />
  );
}

export interface TabItemProps {
  id: string;
  title: string;
  icon?: ReactNode;
  active: boolean;
  dirty?: boolean;
  busy?: boolean;
  preview?: boolean;
  onActivate: () => void;
  onClose: () => void;
  onDoubleClick?: () => void;
  contextItems?: MenuItem[];
  /** Inline rename input instead of the title. */
  renaming?: boolean;
  /** Enter / blur: the new title (`null` when empty = default title). */
  onRename?: (title: string | null) => void;
  /** Esc in the rename input (consumed). */
  onRenameCancel?: () => void;
  /** Tooltip (table tabs: `schema.name @ database`). */
  tooltip?: string;
}

/**
 * One workspace tab: centred 12px title + 14px icon; close button on hover or when active (left
 * on macOS, right on Windows); a dirty dot / busy spinner in its place until hover; preview tabs
 * in italics; middle-click closes; draggable.
 */
export function TabItem({
  id,
  title,
  icon,
  active,
  dirty = false,
  busy = false,
  preview = false,
  onActivate,
  onClose,
  onDoubleClick,
  contextItems,
  renaming = false,
  onRename,
  onRenameCancel,
  tooltip,
}: TabItemProps) {
  const left = isMac();
  const indicator = busy ? (
    <Spinner size={10} tone="current" className="text-fg-secondary" />
  ) : dirty ? (
    <span aria-hidden className="h-2 w-2 rounded-full bg-fg-secondary" />
  ) : null;

  const slot = (
    <span className={cn('absolute top-1/2 flex h-4 w-4 -translate-y-1/2 items-center justify-center', left ? 'left-1.5' : 'right-1.5')}>
      {indicator && <span className="flex items-center justify-center group-hover/tab:hidden">{indicator}</span>}
      <button
        type="button"
        tabIndex={-1}
        aria-label={`${us('closeTab')}: ${title}`}
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
        onDoubleClick={(e) => e.stopPropagation()}
        className={cn(
          'h-4 w-4 items-center justify-center rounded-[4px] text-fg-secondary hover:bg-pressed hover:text-fg',
          active && !indicator ? 'inline-flex' : 'hidden group-hover/tab:inline-flex',
        )}
      >
        <X size={11} strokeWidth={2} />
      </button>
    </span>
  );

  const label = renaming ? (
    <RenameInput title={title} onRename={onRename} onCancel={onRenameCancel} />
  ) : (
    <span className={cn('min-w-0 truncate', preview && 'italic')}>{title}</span>
  );

  const tab = (
    <div
      role="tab"
      aria-selected={active}
      tabIndex={active ? 0 : -1}
      data-roving
      data-tab-id={id}
      draggable={!renaming}
      onDragStart={(e) => {
        e.dataTransfer.setData(DRAG_TYPE, id);
        e.dataTransfer.effectAllowed = 'move';
      }}
      onClick={onActivate}
      onDoubleClick={onDoubleClick}
      onMouseDown={(e) => {
        // No autoscroll cursor on middle-click.
        if (e.button === 1) e.preventDefault();
      }}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          onClose();
        }
      }}
      className={cn(
        'group/tab relative flex h-full min-w-[120px] flex-[1_1_0] select-none items-center justify-center gap-1.5 px-7 text-sm outline-none',
        // Hairlines between tabs (each tab draws its right edge; the sidebar edge is the first left one).
        'border-r border-separator',
        active ? 'bg-window font-medium text-fg' : 'text-fg-secondary hover:bg-hover',
        'focus-visible:shadow-[inset_0_0_0_2px_var(--focus)]',
      )}
    >
      {slot}
      {icon && <span className="inline-flex shrink-0 [&_svg]:size-3.5">{icon}</span>}
      {label}
    </div>
  );

  const withTip = tooltip ? (
    <Tooltip content={tooltip} delay={600}>
      {tab}
    </Tooltip>
  ) : (
    tab
  );
  return contextItems && contextItems.length > 0 ? (
    <ContextMenu items={contextItems} className="flex min-w-[120px] flex-[1_1_0]">
      {withTip}
    </ContextMenu>
  ) : (
    withTip
  );
}
