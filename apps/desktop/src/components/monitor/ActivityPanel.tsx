'use client';

// Activity list (DESIGN.md §5.F): 실행 중 N (newest first) and 최근 결과 (5) + [지우기]. Shown in
// the inspector's 활동 segment and, on Welcome, in the activity popover. Each job is a flat
// `JobRow`; hairlines between jobs, no cards.

import type { ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { cn } from '@/lib/utils';
import { Button, EmptyState } from '@/components/ui';
import { selectActiveJobs, selectRecentResults, useJobsStore } from '@/stores/jobs';
import { useWindowsStore } from '@/stores/windows';
import { JobRow } from './JobRow';
import { ms } from './strings';

function Caption({ title, count, action }: { title: string; count?: number; action?: ReactNode }) {
  return (
    <div className="flex h-(--h-row-sm) shrink-0 items-center gap-1.5 px-3 text-sm">
      <span className="font-semibold text-fg-secondary">{title}</span>
      {count !== undefined && <span className="text-xs tabular-nums text-fg-tertiary">{count}</span>}
      {action && <span className="ml-auto flex items-center">{action}</span>}
    </div>
  );
}

export function ActivityPanel({ variant }: { variant: 'inspector' | 'popover' }) {
  // Newest first: `order` is first-seen order.
  const active = useJobsStore(useShallow((s) => selectActiveJobs(s).reverse()));
  const recent = useJobsStore(useShallow((s) => selectRecentResults(s, 5)));
  const clearFinished = useJobsStore((s) => s.clearFinished);
  // Jobs live per process (docs/db-copy.md 1.2): other BulkDB programs' jobs are not listed.
  const otherInstances = useWindowsStore((s) => s.instances?.others.length ?? 0);

  return (
    <section
      aria-label={ms('title')}
      data-variant={variant}
      className={cn('flex flex-col bg-sidebar', variant === 'popover' ? 'max-h-[min(480px,70vh)] overflow-y-auto' : 'min-h-full')}
    >
      {active.length > 0 ? (
        <>
          <Caption title={ms('running')} count={active.length} />
          <div className="flex flex-col divide-y divide-separator border-y border-separator">
            {active.map((e) => (
              <JobRow key={e.summary.id} entry={e} variant={variant} />
            ))}
          </div>
        </>
      ) : (
        <EmptyState compact title={ms('idle')} className={variant === 'popover' ? 'py-4' : undefined} />
      )}
      {recent.length > 0 && (
        <>
          <Caption
            title={ms('recent')}
            count={recent.length}
            action={
              <Button size="sm" variant="ghost" title={ms('clearTip')} onClick={clearFinished} className="-mr-1.5 text-fg-secondary">
                {ms('clear')}
              </Button>
            }
          />
          <div className="flex flex-col divide-y divide-separator border-t border-separator">
            {recent.map((e) => (
              <JobRow key={e.summary.id} entry={e} variant={variant} />
            ))}
          </div>
        </>
      )}
      {otherInstances > 0 && <p className="px-3 py-2 text-xs text-fg-tertiary">{ms('otherInstances')}</p>}
    </section>
  );
}
