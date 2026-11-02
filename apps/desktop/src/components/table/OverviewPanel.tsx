'use client';

// The 개요 segment (DESIGN.md §5.E): estimated rows (labelled 추정), the exact count with its
// date (the stored last count, or a newer `job:done`) or `—` + [정확히 세기], sizes, kind,
// comment, column and index counts, and a new query tab with the select-all SQL.

import type { ExactCount, TableInfo, TableRef } from 'ipc-types';
import { Calculator, SquarePen } from 'lucide-react';
import type { ReactNode } from 'react';
import { Button, RowEstimate, Tooltip } from '@/components/ui';
import { formatBytes, formatInt } from '@/lib/format';
import type { TableDetails } from '@/stores/catalog';
import { ts } from './strings';
import { ExactCountValue, kindLabel } from './table-ui';

/** Label / value rows: 140px labels (12px secondary), values in 13px. */
export function PropList({ children }: { children: ReactNode }) {
  return <dl className="grid grid-cols-[minmax(96px,140px)_minmax(0,1fr)] items-baseline gap-x-4 gap-y-1.5">{children}</dl>;
}

export function Prop({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <>
      <dt className="text-sm text-fg-secondary">{label}</dt>
      <dd className="min-w-0 text-base" data-selectable>{children}</dd>
    </>
  );
}

export function PropSection({ title, children }: { title: ReactNode; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2 border-b border-separator pb-4 last:border-b-0">
      <h3 className="text-sm font-semibold text-fg-secondary">{title}</h3>
      {children}
    </section>
  );
}

const dash = <span className="text-fg-tertiary">—</span>;

export function OverviewPanel({
  table,
  info,
  details,
  exact,
  counting,
  canQuery,
  onCount,
  onOpenQueryTab,
}: {
  table: TableRef;
  info: TableInfo | undefined;
  details: TableDetails | undefined;
  exact: ExactCount | undefined;
  counting: boolean;
  /** `canTargetTable`: ok, or the reason the query actions are disabled. */
  canQuery: { ok: true } | { ok: false; reason: string };
  onCount: () => void;
  onOpenQueryTab: () => void;
}) {
  const openButton = (
    <Button size="sm" variant="secondary" icon={<SquarePen size={14} strokeWidth={1.75} />} disabled={!canQuery.ok} onClick={onOpenQueryTab}>
      {ts('ov.openQueryTab')}
    </Button>
  );
  return (
    <div className="list-scroll h-full min-h-0 overflow-auto">
      <div className="flex max-w-[680px] flex-col gap-4 px-5 py-4">
        <PropSection title={ts('ov.rows')}>
          <PropList>
            <Prop label={ts('ov.estimated')}>
              <RowEstimate rows={info?.estimatedRows} variant="full" />
            </Prop>
            <Prop label={ts('ov.exact')}>
              <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
                {exact ? <ExactCountValue exact={exact} /> : <span aria-label={ts('ov.notCounted')}>{dash}</span>}
                {!exact && (
                  <Button
                    size="sm"
                    variant="secondary"
                    icon={<Calculator size={14} strokeWidth={1.75} />}
                    loading={counting}
                    onClick={onCount}
                  >
                    {counting ? ts('counting') : ts('countExactly')}
                  </Button>
                )}
                {exact && counting && <span className="text-sm text-fg-secondary">{ts('counting')}</span>}
              </span>
            </Prop>
          </PropList>
          <p className="text-sm text-fg-secondary">{ts('ov.estimateNote')}</p>
        </PropSection>

        <PropSection title={ts('ov.size')}>
          <PropList>
            <Prop label={ts('ov.totalSize')}>
              <span className="tabular-nums">{info?.totalBytes !== undefined ? formatBytes(info.totalBytes) : dash}</span>
            </Prop>
            <Prop label={ts('ov.tableSize')}>
              <span className="tabular-nums">{info?.tableBytes !== undefined ? formatBytes(info.tableBytes) : dash}</span>
            </Prop>
            <Prop label={ts('ov.indexSize')}>
              <span className="tabular-nums">{info?.indexBytes !== undefined ? formatBytes(info.indexBytes) : dash}</span>
            </Prop>
          </PropList>
        </PropSection>

        <PropSection title={ts('ov.table')}>
          <PropList>
            <Prop label={ts('ov.kind')}>{info ? kindLabel(info.kind) : dash}</Prop>
            {table.database && <Prop label={ts('ov.database')}>{table.database}</Prop>}
            {(table.schema ?? info?.schema) && <Prop label={ts('ov.schema')}>{table.schema ?? info?.schema}</Prop>}
            <Prop label={ts('ov.columns')}>
              <span className="tabular-nums">{details?.columns ? formatInt(details.columns.length) : dash}</span>
            </Prop>
            <Prop label={ts('ov.indexes')}>
              <span className="tabular-nums">{details?.indexes ? formatInt(details.indexes.length) : dash}</span>
            </Prop>
            <Prop label={ts('ov.comment')}>{info?.comment ? <span className="whitespace-pre-wrap">{info.comment}</span> : dash}</Prop>
          </PropList>
        </PropSection>

        <PropSection title={ts('ov.query')}>
          <p className="text-sm text-fg-secondary">{ts('ov.queryHint')}</p>
          <div>
            {canQuery.ok ? (
              <Tooltip content={ts('ov.openQueryTabTip')}>{openButton}</Tooltip>
            ) : (
              <Tooltip content={canQuery.reason}>
                <span className="inline-flex">{openButton}</span>
              </Tooltip>
            )}
          </div>
        </PropSection>
      </div>
    </div>
  );
}
