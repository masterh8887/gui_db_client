'use client';

// Status primitives: job state chip, progress bar, row estimate, connection status dot.

import type { ExactCount, JobState } from 'ipc-types';
import { Ban, Check, CircleX, TriangleAlert } from 'lucide-react';
import { formatCompact, formatDuration, formatInt, num } from '@/lib/format';
import { stateLabel } from '@/lib/i18n';
import { isBusyState, STATE_TONE } from '@/lib/job-state';
import { cn } from '@/lib/utils';
import type { ConnStatus } from '@/stores/connections';
import { Spinner } from './Spinner';
import { us } from './strings';
import { Tooltip } from './Tooltip';

const TONE_CLASS = {
  neutral: 'bg-control text-fg-secondary',
  accent: 'bg-accent-soft text-accent',
  success: 'bg-success-soft text-success',
  warning: 'bg-warning-soft text-warning',
  danger: 'bg-danger-soft text-danger',
} as const;

/** One of the nine job states: tone + icon (spinner while busy), never colour alone. */
export function StateChip({ state, className }: { state: JobState; className?: string }) {
  const tone = STATE_TONE[state];
  const icon = isBusyState(state) ? (
    <Spinner size={10} tone="current" decorative />
  ) : state === 'done' ? (
    <Check size={11} strokeWidth={2.25} />
  ) : state === 'cancelled' ? (
    <Ban size={11} strokeWidth={2} />
  ) : state === 'warned' ? (
    <TriangleAlert size={11} strokeWidth={2} />
  ) : (
    <CircleX size={11} strokeWidth={2} />
  );
  return (
    <span
      data-state={state}
      className={cn('inline-flex h-[18px] shrink-0 items-center gap-1 rounded-sm px-1.5 text-sm font-medium leading-none', TONE_CLASS[tone], className)}
    >
      {icon}
      {stateLabel(state)}
    </span>
  );
}

export interface ProgressBarProps {
  /** 0..100; ignored while `indeterminate`. */
  value?: number;
  indeterminate?: boolean;
  tone?: 'accent' | 'warning' | 'danger';
  /** xs 2px, sm 3px. */
  size?: 'xs' | 'sm';
  className?: string;
  'aria-label'?: string;
}

export function ProgressBar({ value, indeterminate = false, tone = 'accent', size = 'sm', className, ...aria }: ProgressBarProps) {
  const fill = tone === 'warning' ? 'bg-warning' : tone === 'danger' ? 'bg-danger' : 'bg-accent';
  const pct = Math.max(0, Math.min(100, value ?? 0));
  return (
    <div
      role="progressbar"
      aria-label={aria['aria-label']}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={indeterminate ? undefined : Math.round(pct)}
      className={cn('relative w-full overflow-hidden rounded-full bg-control', size === 'xs' ? 'h-0.5' : 'h-[3px]', className)}
    >
      {indeterminate ? (
        <div className={cn('animate-indeterminate absolute inset-y-0 w-[35%] rounded-full', fill)} />
      ) : (
        <div className={cn('h-full rounded-full', fill)} style={{ width: `${pct}%` }} />
      )}
    </div>
  );
}

const pad2 = (n: number) => String(n).padStart(2, '0');

/** `10-27` (and ` 13:02` with `time`) in local time. */
function shortDate(iso: string, time: boolean): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const date = `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  return time ? `${date} ${pad2(d.getHours())}:${pad2(d.getMinutes())}` : date;
}

function fullTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

export interface RowEstimateProps {
  rows?: bigint | number;
  exact?: ExactCount;
  /** compact `≈48.2M` (sidebar, palette); full `≈ 48,213,000 (추정)` (overview, bars). */
  variant: 'compact' | 'full';
  /** compact only: append ` 추정`. */
  showLabel?: boolean;
  className?: string;
}

/**
 * A row count that always says what it is (DESIGN.md §2.9): estimates carry ≈ and 추정, an
 * exact count shows when it was counted. Tooltip and aria-label always say 추정 / 정확.
 */
export function RowEstimate({ rows, exact, variant, showLabel = false, className }: RowEstimateProps) {
  if (exact) {
    const n = formatInt(exact.rows);
    const tip = us('exactTip', { rows: n, at: fullTime(exact.at), took: formatDuration(num(exact.elapsedMs)) });
    return (
      <Tooltip content={tip}>
        <span
          aria-label={us('exactAria', { rows: n, at: fullTime(exact.at) })}
          className={cn('sel-muted whitespace-nowrap tabular-nums', className)}
        >
          {n} · {shortDate(exact.at, variant === 'full')}
        </span>
      </Tooltip>
    );
  }
  const n = num(rows);
  if (n === undefined) {
    return (
      <span aria-label={us('noEstimate')} className={cn('sel-muted tabular-nums text-fg-tertiary', className)}>
        —
      </span>
    );
  }
  const tip = us('estimatedTip', { rows: formatInt(n) });
  const text = variant === 'full' ? us('estimatedFull', { rows: formatInt(n) }) : `≈${formatCompact(n)}${showLabel ? ` ${us('estimated')}` : ''}`;
  return (
    <Tooltip content={tip}>
      <span aria-label={us('estimatedAria', { rows: formatInt(n) })} className={cn('sel-muted whitespace-nowrap tabular-nums', className)}>
        {text}
      </span>
    </Tooltip>
  );
}

const DOT_CLASS: Record<ConnStatus, string> = {
  unknown: 'shadow-[inset_0_0_0_1px_var(--fg-tertiary)]',
  testing: 'bg-warning',
  ok: 'bg-success',
  error: 'bg-danger',
};

const STATUS_TEXT: Record<ConnStatus, () => string> = {
  unknown: () => us('statusUnknown'),
  testing: () => us('statusTesting'),
  ok: () => us('statusOk'),
  error: () => us('statusError'),
};

/** Connection test status dot; the tooltip carries the text (error message when given). */
export function StatusDot({ status, title }: { status: ConnStatus; title?: string }) {
  const label = STATUS_TEXT[status]();
  return (
    <Tooltip content={title ?? label}>
      <span role="img" aria-label={label} data-status={status} className={cn('inline-block h-2 w-2 shrink-0 rounded-full', DOT_CLASS[status])} />
    </Tooltip>
  );
}
