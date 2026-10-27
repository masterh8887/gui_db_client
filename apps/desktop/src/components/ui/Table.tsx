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

/** Dense metadata table: 28px rows, 13px text. Never used for data rows (there are none). */
export function Table<T>({ columns, rows, rowKey, empty, className, onRowClick, rowClassName, stickyHeader = true }: TableProps<T>) {
  const alignClass = (a?: Column<T>['align']) => (a === 'right' ? 'text-right' : a === 'center' ? 'text-center' : 'text-left');
  return (
    <table className={cn('w-full border-collapse text-[13px]', className)}>
      <thead className={cn(stickyHeader && 'sticky top-0 z-10')}>
        <tr className="h-7 bg-muted text-xs text-muted-foreground">
          {columns.map((c) => (
            <th
              key={c.id}
              scope="col"
              style={{ width: c.width }}
              className={cn('whitespace-nowrap border-b border-border px-2 font-medium', alignClass(c.align), c.className)}
            >
              {c.header}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.length === 0 ? (
          <tr>
            <td colSpan={columns.length} className="h-7 px-2 text-center text-muted-foreground">
              {empty ?? '—'}
            </td>
          </tr>
        ) : (
          rows.map((r, i) => (
            <tr
              key={rowKey(r, i)}
              onClick={onRowClick ? () => onRowClick(r) : undefined}
              className={cn('h-7 border-b border-border/60 hover:bg-muted/60', onRowClick && 'cursor-pointer', rowClassName?.(r))}
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
