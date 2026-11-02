'use client';

// The 진행 view (docs/db-copy.md 7): overall ProgressBar, rows · size · rate · elapsed · remaining,
// the phase, a filter (전체 / 진행 중 / 실패 / 완료, a native radio row) and one 28px row per table
// (state chip, name, rows written / estimate + %, rate, elapsed, method chip, error in a tooltip).
// [중지] asks first (the same confirmation Activity shows). A job that ended without a result
// (rejected at start, failed before it had one) shows why (`JobEndNotice`). It renders
// `copy.runs[jobId]`, merged by `seq`; it never changes job state itself.

import type { CopyStatus, CopyTableProgress, PlanItem } from 'ipc-types';
import { useMemo, useState } from 'react';
import { Badge, Button, EmptyState, Notice, ProgressBar, Spinner, StateChip, Tooltip } from '@/components/ui';
import { formatBytes, formatByteRate, formatCompact, formatDuration, formatEta, formatInt, formatRowRate, percent } from '@/lib/format';
import { cancelJob } from '@/lib/job-actions';
import { cn } from '@/lib/utils';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { JobEndNotice } from './JobEndNotice';
import { isTerminal, matchesFilter, overallFraction, type TableFilter } from './model';
import { Caption, ItemStateChip, RadioList } from './parts';
import { cs, k } from './strings';

function TableRow({ t, item }: { t: CopyTableProgress; item?: PlanItem }) {
  const pct = percent(t.rowsWritten, t.estRows);
  const name = item ? `${item.targetSchema ? `${item.targetSchema}.` : ''}${item.targetName}` : t.itemId;
  const row = (
    <div className={cn('flex h-7 items-center gap-2 border-t border-separator px-2 text-base first:border-t-0', t.state === 'failed' && 'text-danger')} data-item={t.itemId} data-state={t.state}>
      {/* Wide enough for the longest chip with its spinner ("인덱스 생성 중"). */}
      <span className="w-28 shrink-0">
        <ItemStateChip state={t.state} />
      </span>
      <span className="min-w-0 flex-1 truncate" title={t.itemId}>
        {name}
      </span>
      <span className="w-44 shrink-0 text-right text-sm tabular-nums text-fg-secondary">
        {cs('rowsOf', { done: formatInt(t.rowsWritten), total: t.estRows !== undefined ? `≈${formatCompact(t.estRows)}` : '—' })}
        {pct !== undefined && t.state !== 'skipped' ? ` · ${Math.min(100, Math.round(pct))}%` : ''}
      </span>
      <span className="w-24 shrink-0 text-right text-sm tabular-nums text-fg-secondary">{t.rateRowsPerSec > 0 ? formatRowRate(t.rateRowsPerSec) : ''}</span>
      <span className="w-14 shrink-0 text-right text-sm tabular-nums text-fg-secondary">{t.elapsedMs !== undefined ? formatDuration(t.elapsedMs) : ''}</span>
      <span className="w-20 shrink-0 text-right">{t.method && <Badge tone="neutral">{cs(k(`method.${t.method}`))}</Badge>}</span>
    </div>
  );
  if (!t.error) return row;
  // The error is read inline, not only in a hover tooltip, so keyboard and screen-reader users see it.
  const err = cs('tableError', { code: t.error.code, message: t.error.message });
  return (
    <>
      <Tooltip content={err}>{row}</Tooltip>
      <div className="truncate px-2 pb-1 pl-[7.5rem] text-sm text-danger" data-error={t.itemId}>
        {err}
      </div>
    </>
  );
}

export function ProgressView({ status }: { status: CopyStatus | undefined }) {
  const [filter, setFilter] = useState<TableFilter>('all');
  const jobId = status?.jobId ?? '';
  const job = useJobsStore((s) => (jobId ? s.jobs[jobId] : undefined));
  const confirming = useUiStore((s) => s.cancelConfirmJobId === jobId && !!jobId);
  const state = job?.summary.state ?? status?.state;
  const progress = status?.progress;
  const items = useMemo(() => new Map((status?.plan?.items ?? []).map((i) => [i.id, i])), [status?.plan]);
  if (!status) {
    return (
      <div className="flex items-center gap-2 text-fg-secondary" role="status">
        <Spinner size={14} decorative />
        {cs('planning')}
      </div>
    );
  }
  const frac = overallFraction(progress);
  const jp = job?.progress;
  const tables = progress?.tables ?? [];
  const shown = tables.filter((t) => matchesFilter(t, filter));
  const active = !isTerminal(state);
  // Ended without a result: say why; with no table listed there is no progress to show either.
  const endedEmpty = !active && !status.result;
  const bare = endedEmpty && tables.length === 0;
  const bytes = tables.reduce((n, t) => n + Number(t.bytes), 0);
  const rows = tables.reduce((n, t) => n + (t.state === 'skipped' ? 0 : Number(t.rowsWritten)), 0);
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        {state && <StateChip state={state} />}
        <span className="font-medium">{progress ? cs(k(`phase.${progress.phase}`)) : ''}</span>
        {progress && <span className="text-sm tabular-nums text-fg-secondary">{cs('tablesProgress', { done: progress.tablesDone, total: progress.tablesTotal })}</span>}
        {active && !confirming && (
          // The confirmation shows inline here; `askCancel` would also pop Activity open over the sheet.
          <Button size="sm" variant="secondary" className="ml-auto" onClick={() => useUiStore.getState().setCancelConfirm(jobId)}>
            {cs('stop')}
          </Button>
        )}
      </div>
      {confirming && (
        <Notice tone="warning">
          <div role="alertdialog" aria-label={cs('stopConfirm')} className="flex flex-wrap items-center gap-2">
            <span className="font-medium">{cs('stopConfirm')}</span>
            <span className="text-sm text-fg-secondary">{cs('stopNote')}</span>
            <span className="ml-auto flex gap-1.5">
              <Button size="sm" variant="dangerFilled" onClick={() => void cancelJob(jobId)}>
                {cs('yes')}
              </Button>
              <Button size="sm" variant="secondary" autoFocus onClick={() => useUiStore.getState().setCancelConfirm(null)}>
                {cs('no')}
              </Button>
            </span>
          </div>
        </Notice>
      )}
      {endedEmpty && <JobEndNotice jobId={jobId} state={state} />}
      {bare ? null : !status.plan && active ? (
        <div className="flex items-center gap-2 text-fg-secondary" role="status">
          <Spinner size={14} decorative />
          {cs('planning')}
        </div>
      ) : (
        <>
          <ProgressBar value={frac === undefined ? undefined : frac * 100} indeterminate={frac === undefined && active} aria-label={cs('step.progress')} />
          <div className="flex flex-wrap gap-x-3 text-sm tabular-nums text-fg-secondary">
            <span>
              {cs('progressStats', {
                rows: formatCompact(rows),
                size: formatBytes(bytes),
                rate: jp ? formatByteRate(jp.rateBytesPerSec) : '—',
                elapsed: formatDuration(jp?.elapsedMs ?? 0),
              })}
            </span>
            {active && jp?.etaSec !== undefined && <span>{cs('progressEta', { eta: formatEta(jp.etaSec) })}</span>}
          </div>
        </>
      )}
      {!bare && (
        <>
          <div className="flex items-center gap-2">
            <Caption>{cs('tablesAria')}</Caption>
            <div className="ml-auto">
              <RadioList
                inline
                name="filter"
                label={cs('filterAria')}
                value={filter}
                onChange={setFilter}
                options={(['all', 'active', 'failed', 'done'] as const).map((v) => ({ value: v, label: cs(k(`filter.${v}`)) }))}
              />
            </div>
          </div>
          <div className="max-h-[360px] overflow-auto border-y border-separator" role="list" aria-label={cs('tablesAria')}>
            {shown.length === 0 ? (
              <EmptyState compact title={cs('noTablesInFilter')} />
            ) : (
              shown.map((t) => (
                <div role="listitem" key={t.itemId}>
                  <TableRow t={t} item={items.get(t.itemId)} />
                </div>
              ))
            )}
          </div>
        </>
      )}
    </div>
  );
}
