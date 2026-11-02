'use client';

// The title of a job in Activity, JobChip's tooltip and the status pill (DESIGN.md §5.F). A copy
// job reads "복사: {src}.{db} → {tgt}.{db}" (docs/db-copy.md 7 "Activity"); in the ~300px rows of
// the inspector and the activity popover the title truncates in the middle: the ends share a short
// line 2 : 3 in favour of the target (the side being written), connection names get the ellipsis
// and both `.database` parts stay readable. The full text is the tooltip.

import type { JobSummary } from 'ipc-types';
import { cn } from '@/lib/utils';
import { ms } from './strings';

interface End {
  conn: string;
  db: string;
}

function copyEndParts(summary: JobSummary): { source: End; target: End } | null {
  const c = summary.copy;
  if (summary.kind !== 'copy' || !c) return null;
  return {
    source: { conn: summary.connName ?? summary.connId, db: c.sourceDatabase },
    target: { conn: c.targetConnName || c.targetConnId, db: c.targetDatabase },
  };
}

/** Both ends of a copy job, `connection.database`; `null` for other jobs (or a stub without `copy`). */
export function copyEnds(summary: JobSummary): { source: string; target: string } | null {
  const p = copyEndParts(summary);
  return p ? { source: `${p.source.conn}.${p.source.db}`, target: `${p.target.conn}.${p.target.db}` } : null;
}

/** The job's title as one string (tooltips, aria labels). */
export function jobTitle(summary: JobSummary): string {
  const ends = copyEnds(summary);
  return ends ? ms('copyTitle', ends) : summary.label;
}

/** `connection` (truncates) + `.database` (stays). */
function EndText({ end, side, className }: { end: End; side: 'source' | 'target'; className: string }) {
  return (
    <span data-end={side} className={cn('flex min-w-0 overflow-hidden', className)}>
      <span className="min-w-0 truncate">{end.conn}</span>
      <span className="shrink-0">.{end.db}</span>
    </span>
  );
}

export function JobTitle({ summary, className }: { summary: JobSummary; className?: string }) {
  const parts = copyEndParts(summary);
  const full = jobTitle(summary);
  if (!parts) {
    return (
      <span className={cn('min-w-0 flex-1 truncate text-base font-medium', className)} title={full}>
        {full}
      </span>
    );
  }
  return (
    <span data-testid="copy-job-title" className={cn('flex min-w-0 flex-1 items-baseline text-base font-medium', className)} title={full}>
      <span className="shrink-0 pr-1 text-fg-secondary">{ms('copyPrefix')}</span>
      {/* Middle truncation: when the line is short the ends share it 2 : 3 (never more than their
          own width, the rest goes to the other end), and each keeps its `.database`:
          "복사: legacy….shop → mysql8 (My….shop". */}
      <EndText end={parts.source} side="source" className="max-w-max flex-[2_1_0%]" />
      <span aria-hidden className="shrink-0 px-1 text-fg-tertiary">
        →
      </span>
      <EndText end={parts.target} side="target" className="max-w-max flex-[3_1_0%]" />
    </span>
  );
}
