'use client';

// Right pane: active jobs (phase, elapsed, rows, MB/s, ETA, server state, cancel) and, when
// idle, the last results.

import { Activity, Trash2 } from 'lucide-react';
import { useShallow } from 'zustand/react/shallow';
import { t } from '@/lib/i18n';
import { EmptyState, IconButton } from '@/components/ui';
import { selectActiveJobs, selectRecentResults, useJobsStore } from '@/stores/jobs';
import { JobCard } from './JobCard';

export function MonitorPanel() {
  const active = useJobsStore(useShallow(selectActiveJobs));
  const recent = useJobsStore(useShallow((s) => selectRecentResults(s, 5)));
  const clearFinished = useJobsStore((s) => s.clearFinished);

  return (
    <section className="flex h-full flex-col bg-background" aria-label={t('monitor.title')}>
      <header className="flex h-7 shrink-0 items-center gap-1 border-b border-border bg-muted pl-2 pr-1">
        <Activity size={13} className="text-muted-foreground" />
        <span className="text-xs font-medium text-muted-foreground">{t('monitor.title')}</span>
        {active.length > 0 && <span className="rounded-full bg-accent px-1.5 text-[10px] font-semibold text-accent-foreground">{active.length}</span>}
        <IconButton label={t('monitor.clearFinished')} size="sm" className="ml-auto" onClick={clearFinished} disabled={recent.length === 0}>
          <Trash2 size={12} />
        </IconButton>
      </header>
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-auto p-2">
        {active.length > 0 ? (
          <>
            <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{t('monitor.active')}</h3>
            {active.map((e) => (
              <JobCard key={e.summary.id} entry={e} />
            ))}
          </>
        ) : (
          <EmptyState compact title={t('monitor.idle')} />
        )}
        {recent.length > 0 && (
          <>
            <h3 className="mt-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{t('monitor.recent')}</h3>
            {recent.map((e) => (
              <JobCard key={e.summary.id} entry={e} compact />
            ))}
          </>
        )}
      </div>
    </section>
  );
}
