'use client';

// The 열 and 인덱스 segments of the structure view (DESIGN.md §5.E): dense grids, sans names
// (index 열 / 포함 column lists too, DESIGN.md §3.3), mono types / defaults / conditions only.
// `Mod+C` copies the selected row's name.

import type { ColumnInfo, IndexInfo, TableRef } from 'ipc-types';
import { KeyRound } from 'lucide-react';
import { useState, type KeyboardEvent } from 'react';
import { Badge, Tooltip, type MenuItem } from '@/components/ui';
import { formatBytes, formatInt } from '@/lib/format';
import { hasModKey } from '@/lib/platform';
import { copyColumnList } from '@/lib/table-actions';
import { copyText, tableLabel } from '@/lib/utils';
import { MetaGrid, type GridColumn } from './MetaGrid';
import { ts } from './strings';
import { SEL_CHIP } from './table-ui';

/** `Mod+C` on a selected row copies `text` (no toast: DESIGN.md §3.3). */
function copyKey(e: KeyboardEvent, text: string | undefined) {
  if (!text || !hasModKey(e) || e.shiftKey || e.altKey || e.key.toLowerCase() !== 'c') return;
  // Selected page text (a DDL selection elsewhere) keeps the native copy.
  if (window.getSelection()?.toString()) return;
  e.preventDefault();
  void copyText(text);
}

const muted = 'sel-muted text-fg-secondary';
const mono = 'font-mono text-sm';
/** Long free text: truncated, the full text in the tooltip. */
function LongText({ text, className }: { text: string | undefined; className?: string }) {
  if (!text) return null;
  return (
    <Tooltip content={text}>
      <span className={`block max-w-[360px] truncate ${className ?? ''}`}>{text}</span>
    </Tooltip>
  );
}

function columnDefs(): GridColumn<ColumnInfo>[] {
  return [
    { id: 'pos', header: ts('col.pos'), width: 40, align: 'right', className: `tabular-nums text-sm ${muted}`, cell: (c) => c.position },
    {
      id: 'name',
      header: ts('col.name'),
      cell: (c) => (
        <span className="flex items-center gap-1.5">
          <span className="inline-flex w-3 shrink-0 justify-center">
            {c.primaryKey && <KeyRound size={12} strokeWidth={2} role="img" aria-label={ts('primaryKey')} className={muted} />}
          </span>
          <span className={c.primaryKey ? 'font-medium' : undefined}>{c.name}</span>
        </span>
      ),
    },
    { id: 'type', header: ts('col.type'), className: `${mono} ${muted}`, cell: (c) => c.dataType },
    {
      id: 'null',
      header: ts('col.nullable'),
      className: 'text-xs',
      cell: (c) => (c.nullable ? <span className="sel-muted text-fg-tertiary">{ts('nullable')}</span> : <span className={muted}>{ts('notNull')}</span>),
    },
    { id: 'default', header: ts('col.default'), className: mono, cell: (c) => <LongText text={c.default} /> },
    {
      id: 'key',
      header: ts('col.key'),
      cell: (c) => (
        <span className="flex items-center gap-1">
          {c.primaryKey && <Badge tone="accent" className={SEL_CHIP}>PK</Badge>}
          {c.unique && !c.primaryKey && <Badge tone="neutral" className={SEL_CHIP}>UQ</Badge>}
          {c.foreignKey && (
            <Tooltip content={ts('fkTip', { target: c.foreignKey })}>
              <span className="inline-flex" aria-label={ts('fkTip', { target: c.foreignKey })}>
                <Badge tone="neutral" className={SEL_CHIP}>FK</Badge>
              </span>
            </Tooltip>
          )}
        </span>
      ),
    },
    { id: 'comment', header: ts('col.comment'), className: `text-sm ${muted}`, cell: (c) => <LongText text={c.comment} /> },
  ];
}

export function ColumnsGrid({ connId, table, columns }: { connId: string; table: TableRef; columns: ColumnInfo[] }) {
  const [active, setActive] = useState(-1);
  const menu = (c: ColumnInfo): MenuItem[] => [
    { id: 'copyName', label: ts('copyName'), shortcut: 'mod+c', onSelect: () => void copyText(c.name) },
    { id: 'copyColumns', label: ts('copyColumns'), onSelect: () => void copyColumnList(connId, table) },
  ];
  return (
    <MetaGrid
      aria-label={ts('columnsGrid', { table: tableLabel(table) })}
      columns={columnDefs()}
      rows={columns}
      rowKey={(c) => `${c.position}:${c.name}`}
      activeIndex={active}
      onMove={setActive}
      rowContextItems={menu}
      onRowKeyDown={(e, i) => copyKey(e, columns[i]?.name)}
      empty={ts('noColumns')}
    />
  );
}

function indexDefs(): GridColumn<IndexInfo>[] {
  return [
    { id: 'name', header: ts('idx.name'), cell: (i) => <span className={i.primary ? 'font-medium' : undefined}>{i.name}</span> },
    { id: 'cols', header: ts('idx.columns'), cell: (i) => i.columns.join(', ') },
    { id: 'kind', header: ts('idx.kind'), className: `text-sm ${muted}`, cell: (i) => i.kind ?? '' },
    {
      id: 'unique',
      header: ts('idx.unique'),
      cell: (i) => (
        <span className="flex gap-1">
          {i.primary && <Badge tone="accent" className={SEL_CHIP}>PK</Badge>}
          {i.unique && !i.primary && <Badge tone="neutral" className={SEL_CHIP}>UNIQUE</Badge>}
        </span>
      ),
    },
    {
      id: 'pred',
      header: ts('idx.predicate'),
      cell: (i) => (i.predicate ? <LongText text={`WHERE ${i.predicate}`} className={`${mono} sel-muted text-warning`} /> : null),
    },
    { id: 'include', header: ts('idx.include'), className: muted, cell: (i) => i.include.join(', ') },
    { id: 'size', header: ts('idx.size'), align: 'right', className: `tabular-nums ${muted}`, cell: (i) => formatBytes(i.sizeBytes) },
    { id: 'scans', header: ts('idx.scans'), align: 'right', className: `tabular-nums ${muted}`, cell: (i) => formatInt(i.scans) },
  ];
}

export function IndexesGrid({ table, indexes }: { table: TableRef; indexes: IndexInfo[] }) {
  const [active, setActive] = useState(-1);
  const menu = (i: IndexInfo): MenuItem[] => [
    { id: 'copyName', label: ts('copyName'), shortcut: 'mod+c', onSelect: () => void copyText(i.name) },
    { id: 'copyColumns', label: ts('copyIndexColumns'), onSelect: () => void copyText(i.columns.join(', ')) },
    ...(i.definition ? [{ id: 'copyDefinition', label: ts('copyDefinition'), onSelect: () => void copyText(i.definition ?? '') }] : []),
  ];
  return (
    <MetaGrid
      aria-label={ts('indexesGrid', { table: tableLabel(table) })}
      columns={indexDefs()}
      rows={indexes}
      rowKey={(i) => i.name}
      activeIndex={active}
      onMove={setActive}
      rowContextItems={menu}
      onRowKeyDown={(e, i) => copyKey(e, indexes[i]?.name)}
      empty={ts('noIndexes')}
    />
  );
}
