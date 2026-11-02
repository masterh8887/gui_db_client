'use client';

// The terminal error strip of a job (DESIGN.md §5.F "Terminal"): danger-soft, warning-soft when
// the job was cancelled; state icon, the error code in a 12px mono code chip (§3.3), the message
// and the detail (kept line breaks). One component so a failed job reads the same in Activity
// (JobRow) and in its own query tab (query/JobView), which can render it as is.

import type { ErrorPayload, JobState } from 'ipc-types';
import { Ban, CircleX } from 'lucide-react';
import { cn } from '@/lib/utils';

/** Same box as `query/parts` `CodeChip`: 18px high, 4px radius, mono 12px (`text-sm`). */
const CODE_CHIP =
  'inline-flex h-[18px] shrink-0 items-center rounded-sm bg-control px-1.5 font-mono text-sm text-fg-secondary';

export function JobErrorStrip({
  error,
  state,
  role,
  className,
}: {
  error: ErrorPayload;
  state: JobState;
  /** `alert` where the strip appears as the job ends (a job tab); a list of past jobs leaves it out. */
  role?: 'alert';
  className?: string;
}) {
  const cancelled = state === 'cancelled';
  const Icon = cancelled ? Ban : CircleX;
  return (
    <div
      role={role}
      data-selectable
      className={cn(
        'flex flex-col gap-0.5 rounded-md px-2.5 py-1.5',
        cancelled ? 'bg-warning-soft' : 'bg-danger-soft',
        className,
      )}
    >
      <div className="flex items-start gap-2">
        <Icon
          size={14}
          strokeWidth={1.75}
          aria-hidden
          className={cn('mt-0.5 shrink-0', cancelled ? 'text-warning' : 'text-danger')}
        />
        <span className={CODE_CHIP}>{error.code}</span>
        <span className="min-w-0 flex-1 break-words">{error.message}</span>
      </div>
      {error.detail && <div className="whitespace-pre-wrap break-words text-sm text-fg-secondary">{error.detail}</div>}
    </div>
  );
}
