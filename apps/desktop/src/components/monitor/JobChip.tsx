'use client';

// The global job indicator (DESIGN.md §5.F "JobChip"): a 28px ghost button, rendered only while
// the jobs store holds jobs. Active: spinner + the primary job's percent (or phase) + `+N`;
// warned: `TriangleAlert` + "경고 확인 필요"; only finished jobs: a quiet state icon. Click →
// `showActivity()` (workspace: the inspector's 활동; Welcome: the activity popover).

import { Ban, Check, CircleX, TriangleAlert } from 'lucide-react';
import { useRef, type ReactNode } from 'react';
import { formatClock, formatCompact, num } from '@/lib/format';
import { phaseLabel, stateLabel } from '@/lib/i18n';
import { showActivity } from '@/lib/job-actions';
import { jobProgressPercent } from '@/lib/job-state';
import { cn } from '@/lib/utils';
import { Spinner, Tooltip } from '@/components/ui';
import { isActiveState, selectLastResult, selectPrimaryActiveJob, useJobsStore, type JobEntry, type JobsState } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { ActivityPopover, closeActivityPopover } from './ActivityPopover';
import { jobTitle } from './JobTitle';
import { ms } from './strings';

function activeCount(s: JobsState): number {
  let n = 0;
  for (const id of s.order) if (s.jobs[id] && isActiveState(s.jobs[id].summary.state)) n += 1;
  return n;
}

/** What the chip says about the primary active job (`41%`, a phase, or the state). */
export function chipText(e: JobEntry): string {
  const { state } = e.summary;
  if (state === 'warned') return ms('chipWarned');
  if (state === 'running') {
    const pct = jobProgressPercent(e.progress);
    if (pct !== undefined) return `${Math.floor(pct)}%`;
    return e.progress ? phaseLabel(e.progress.phase) : stateLabel(state);
  }
  if (state === 'finishing') return phaseLabel('finishing');
  return stateLabel(state);
}

/** Tooltip: label, rows, ETA (or the state). */
export function chipTooltip(e: JobEntry): string {
  const { summary, progress } = e;
  const parts = [jobTitle(summary)];
  if (summary.state === 'warned') parts.push(ms('chipWarned'));
  else if (summary.state === 'running' && progress) {
    parts.push(ms('rows', { n: formatCompact(progress.rows) }));
    const eta = num(progress.etaSec);
    if (eta !== undefined) parts.push(ms('eta', { t: formatClock(eta) }));
  } else parts.push(stateLabel(summary.state));
  return parts.join(' · ');
}

export function JobChip({ variant }: { variant: 'toolbar' | 'welcome' }) {
  const any = useJobsStore((s) => s.order.length > 0);
  const primary = useJobsStore((s) => selectPrimaryActiveJob(s));
  const count = useJobsStore(activeCount);
  const last = useJobsStore((s) => selectLastResult(s));
  const popoverOpen = useUiStore((s) => s.activityPopoverOpen);
  const anchor = useRef<HTMLButtonElement>(null);
  if (!any) return null;

  let icon: ReactNode = null;
  let text: string | null = null;
  let tip = ms('chipTip');
  if (primary) {
    const warned = primary.summary.state === 'warned';
    icon = warned ? (
      <TriangleAlert size={14} strokeWidth={2} className="text-warning" />
    ) : (
      <Spinner size={12} tone="current" />
    );
    text = chipText(primary);
    tip = chipTooltip(primary);
  } else if (last) {
    const s = last.summary.state;
    icon =
      s === 'done' ? (
        <Check size={15} strokeWidth={2} className="text-success" />
      ) : s === 'cancelled' ? (
        <Ban size={14} strokeWidth={1.75} />
      ) : (
        <CircleX size={14} strokeWidth={1.75} className="text-danger" />
      );
    tip = chipTooltip(last);
  }
  const more = primary && count > 1 ? ms('chipMore', { n: count - 1 }) : null;

  const onClick = () => {
    if (variant === 'welcome' && popoverOpen) closeActivityPopover();
    else showActivity();
  };

  return (
    <>
      <Tooltip content={tip} delay={300}>
        <button
          ref={anchor}
          type="button"
          aria-label={`${ms('chipTip')}: ${tip}${more ? ` (${more})` : ''}`}
          aria-haspopup={variant === 'welcome' ? 'dialog' : undefined}
          aria-expanded={variant === 'welcome' ? popoverOpen : undefined}
          data-job-state={primary?.summary.state ?? last?.summary.state}
          onClick={onClick}
          className={cn(
            'inline-flex h-7 max-w-[220px] shrink-0 items-center gap-1.5 rounded-md text-base tabular-nums text-fg-secondary transition-colors duration-[80ms] hover:bg-hover hover:text-fg active:bg-pressed focus-visible:outline-2 focus-visible:outline-focus',
            text ? 'px-2' : 'w-7 justify-center',
            variant === 'welcome' && popoverOpen && 'bg-control',
          )}
        >
          <span className="inline-flex shrink-0 items-center">{icon}</span>
          {text && <span className="min-w-0 truncate">{text}</span>}
          {more && <span className="shrink-0 text-sm text-fg-tertiary">{more}</span>}
        </button>
      </Tooltip>
      {variant === 'welcome' && <ActivityPopover anchor={anchor} />}
    </>
  );
}
