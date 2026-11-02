'use client';

// Small building blocks shared by the export panel views and the preflight sheet: form rows with
// 120px right-aligned labels, mono code chips, section captions and the panel empty state.

import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { Tooltip } from '@/components/ui';

/** Middle-truncate `text` to at most `max` characters (`D:\exports\pro…_20261027.csv`). */
export function middleTruncate(text: string, max = 64): string {
  if (text.length <= max) return text;
  const keep = max - 1;
  const head = Math.ceil(keep / 2);
  return `${text.slice(0, head)}…${text.slice(text.length - (keep - head))}`;
}

/** A form row: 120px right-aligned label, then the control (and an optional hint under it). */
export function FormRow({
  label,
  htmlFor,
  hint,
  title,
  children,
  className,
}: {
  label: ReactNode;
  htmlFor?: string;
  hint?: ReactNode;
  /** Tooltip over the whole row (e.g. why it is disabled). */
  title?: string;
  children: ReactNode;
  className?: string;
}) {
  const row = (
    <div className={cn('grid grid-cols-[120px_minmax(0,1fr)] items-start gap-x-3', className)}>
      <label htmlFor={htmlFor} className="flex min-h-(--h-control) items-center justify-end text-right text-base text-fg-secondary">
        {label}
      </label>
      <div className="flex min-w-0 flex-col gap-1">
        {children}
        {hint && <div className="text-sm text-fg-secondary">{hint}</div>}
      </div>
    </div>
  );
  return title ? <Tooltip content={title}>{row}</Tooltip> : row;
}

/** A mono 12px chip for codes (`WORK_MEM_HIGH`, `MULTI_STATEMENT`). */
export function CodeChip({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <span
      data-selectable
      className={cn('inline-flex h-[18px] shrink-0 items-center rounded-sm bg-control px-1.5 font-mono text-sm text-fg-secondary', className)}
    >
      {children}
    </span>
  );
}

/** 12px/600 secondary caption (no uppercase tracking). */
export function Caption({ children, className, tone }: { children: ReactNode; className?: string; tone?: 'danger' | 'warning' }) {
  return (
    <h3
      className={cn(
        'text-sm font-semibold',
        tone === 'danger' ? 'text-danger' : tone === 'warning' ? 'text-warning' : 'text-fg-secondary',
        className,
      )}
    >
      {children}
    </h3>
  );
}

/** Empty state of an export panel view: a 13px secondary line, a 12px hint and an action. */
export function PanelEmpty({ title, hint, action }: { title: string; hint?: string; action?: ReactNode }) {
  return (
    <div className="flex h-full min-h-24 flex-col items-center justify-center gap-1.5 px-6 text-center">
      <div className="text-base text-fg-secondary">{title}</div>
      {hint && <div className="max-w-md text-sm text-fg-secondary">{hint}</div>}
      {action && <div className="mt-1.5">{action}</div>}
    </div>
  );
}
