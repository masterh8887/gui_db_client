import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface Column<T> {
  id: string;
  header: ReactNode;
  cell: (row: T, index: number) => ReactNode;
  className?: string;
  align?: 'left' | 'right' | 'center';
  width?: number | string;
}

export interface TableProps<T> {
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  empty?: ReactNode;
  className?: string;
  onRowClick?: (row: T) => void;
  rowClassName?: (row: T) => string | undefined;
  /** Sticky header when the parent scrolls. */
  stickyHeader?: boolean;
}

/**
 * Metadata table: 26px sticky `bg-panel` header (12px secondary) and 26px rows with hairlines,
 * no zebra, no vertical lines. Never used for data rows (there are none).
 */
export function Table<T>({ columns, rows, rowKey, empty, className, onRowClick, rowClassName, stickyHeader = true }: TableProps<T>) {
  const alignClass = (a?: Column<T>['align']) => (a === 'right' ? 'text-right' : a === 'center' ? 'text-center' : 'text-left');
  return (
    <table className={cn('w-full border-collapse text-base', className)}>
      <thead className={cn(stickyHeader && 'sticky top-0 z-10')}>
        <tr className="h-(--h-row) bg-panel text-sm text-fg-secondary">
          {columns.map((c) => (
            <th
              key={c.id}
              scope="col"
              style={{ width: c.width }}
              className={cn('whitespace-nowrap border-b border-separator px-2 font-medium', alignClass(c.align), c.className)}
            >
              {c.header}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.length === 0 ? (
          <tr>
            <td colSpan={columns.length} className="h-(--h-row) px-2 text-center text-fg-secondary">
              {empty ?? '—'}
            </td>
          </tr>
        ) : (
          rows.map((r, i) => (
            <tr
              key={rowKey(r, i)}
              onClick={onRowClick ? () => onRowClick(r) : undefined}
              className={cn('h-(--h-row) border-b border-separator', onRowClick && 'hover:bg-hover', rowClassName?.(r))}
            >
              {columns.map((c) => (
                <td key={c.id} className={cn('truncate px-2 align-middle', alignClass(c.align), c.className)}>
                  {c.cell(r, i)}
                </td>
              ))}
            </tr>
          ))
        )}
      </tbody>
    </table>
  );
}
