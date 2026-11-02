'use client';

// Dense metadata grid (structure columns / indexes, the history tab): 26px sticky `bg-panel`
// header, 26px hairline rows, no zebra, no vertical lines (DESIGN.md §5.E). Keyboard: the grid
// is one tab stop with roving selection (`↑ ↓ Home End`, `Enter` opens); the selected row is
// `bg-selected`, or accent-filled while the grid has focus (the `[data-roving-list]` rule in
// globals.css). Never used for table data: BulkDB shows none.

import { useRef, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react';
import { ContextMenu, useListNavigation, type MenuItem } from '@/components/ui';
import { cn } from '@/lib/utils';

export interface GridColumn<T> {
  id: string;
  header: ReactNode;
  cell: (row: T, index: number) => ReactNode;
  align?: 'left' | 'right' | 'center';
  width?: number | string;
  /** Classes of the body cells. */
  className?: string;
  /** The header text is for screen readers only (an action column). */
  srOnlyHeader?: boolean;
}

export interface MetaGridProps<T> {
  columns: GridColumn<T>[];
  rows: readonly T[];
  rowKey: (row: T, index: number) => string;
  'aria-label': string;
  /** Selected row, -1 for none. */
  activeIndex: number;
  /** Move the selection (arrows, Home, End, click, right-click). */
  onMove: (index: number) => void;
  /** `Enter`, double-click, or a single click with `openOnClick`. */
  onOpen?: (index: number) => void;
  /** A click opens the row (history tab); double-clicks never open twice. */
  openOnClick?: boolean;
  /** Context menu of a row (right-click selects the row first). */
  rowContextItems?: (row: T, index: number) => MenuItem[];
  /** Extra keys on the selected row; `preventDefault()` there skips the built-in handling. */
  onRowKeyDown?: (e: KeyboardEvent<HTMLTableElement>, index: number) => void;
  /** Shown in a single row when there are no rows. */
  empty?: ReactNode;
  className?: string;
}

const alignClass = (a?: 'left' | 'right' | 'center') => (a === 'right' ? 'text-right' : a === 'center' ? 'text-center' : 'text-left');

export function MetaGrid<T>({
  columns,
  rows,
  rowKey,
  activeIndex,
  onMove,
  onOpen,
  openOnClick = false,
  rowContextItems,
  onRowKeyDown,
  empty,
  className,
  ...aria
}: MetaGridProps<T>) {
  const tableRef = useRef<HTMLTableElement>(null);
  const nav = useListNavigation({ count: rows.length, activeIndex, onMove, onOpen });
  const active = activeIndex >= 0 && activeIndex < rows.length ? activeIndex : -1;

  const onKeyDown = (e: KeyboardEvent<HTMLTableElement>) => {
    if (active >= 0) onRowKeyDown?.(e, active);
    if (e.defaultPrevented) return;
    nav.onKeyDown(e);
  };

  const onRowClick = (e: MouseEvent, i: number) => {
    onMove(i);
    // The second click of a double-click must not open a second tab.
    if (openOnClick && e.detail <= 1) onOpen?.(i);
  };

  const table = (
    <table
      ref={tableRef}
      role="grid"
      aria-label={aria['aria-label']}
      aria-rowcount={rows.length + 1}
      aria-activedescendant={active >= 0 ? nav.itemId(active) : undefined}
      tabIndex={0}
      data-roving-list=""
      onKeyDown={onKeyDown}
      className={cn(
        'w-full border-collapse text-base outline-none',
        // Nothing selected yet: the grid itself shows that it has focus.
        active < 0 && rows.length > 0 && 'focus-visible:shadow-[inset_0_0_0_2px_var(--focus)]',
      )}
    >
      <thead className="sticky top-0 z-10" onContextMenu={(e) => e.preventDefault()}>
        <tr role="row" className="h-(--h-row) bg-panel text-sm text-fg-secondary">
          {columns.map((c) => (
            <th
              key={c.id}
              role="columnheader"
              scope="col"
              style={{ width: c.width }}
              className={cn('whitespace-nowrap border-b border-separator px-2 font-medium', alignClass(c.align))}
            >
              {c.srOnlyHeader ? <span className="sr-only">{c.header}</span> : c.header}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.length === 0 ? (
          <tr role="row" className="h-(--h-row)">
            <td role="gridcell" colSpan={columns.length} className="px-2 text-center text-fg-secondary">
              {empty ?? '—'}
            </td>
          </tr>
        ) : (
          rows.map((r, i) => (
            <tr
              key={rowKey(r, i)}
              id={nav.itemId(i)}
              role="row"
              aria-selected={i === active}
              onClick={(e) => onRowClick(e, i)}
              onDoubleClick={openOnClick ? undefined : () => onOpen?.(i)}
              onContextMenu={() => {
                onMove(i);
                tableRef.current?.focus({ preventScroll: true });
              }}
              className="group/row h-(--h-row) border-b border-separator aria-selected:bg-selected [&:not([aria-selected=true]):hover]:bg-hover"
            >
              {columns.map((c) => (
                <td key={c.id} role="gridcell" className={cn('whitespace-nowrap px-2 align-middle', alignClass(c.align), c.className)}>
                  {c.cell(r, i)}
                </td>
              ))}
            </tr>
          ))
        )}
      </tbody>
    </table>
  );

  // One menu for the whole grid: a right-click selects its row first (same render batch as the
  // menu opening), so the items always belong to the row under the pointer. The items are never
  // empty while there are rows (an empty list would swap the menu wrapper and remount the grid).
  const menuRow = active >= 0 ? active : 0;
  const items = rowContextItems && rows.length > 0 ? rowContextItems(rows[menuRow], menuRow) : [];
  return (
    <ContextMenu items={items} disabled={!rowContextItems || rows.length === 0} className={cn('list-scroll h-full min-h-0 overflow-auto', className)}>
      {table}
    </ContextMenu>
  );
}
