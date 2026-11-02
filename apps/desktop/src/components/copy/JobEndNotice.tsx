'use client';

// Why a copy job ended badly (docs/db-copy.md 5.1): core puts the reason of a rejected or failed
// job only in the job's error (`JobSummary.error` / `JobDone.error`), so the 진행 / 결과 views read
// it from the jobs store. A rejected copy lists its blockers as "CODE: message" lines in `detail`:
// each line shows its title (`issue.<CODE>`) and code chip.

import type { JobState } from 'ipc-types';
import { Notice } from '@/components/ui';
import { useJobsStore } from '@/stores/jobs';
import { errorLines, issueTitle } from './model';
import { CodeChip } from './parts';
import { cs, k } from './strings';

const ENDED: readonly JobState[] = ['rejected', 'failed', 'cancelled'];

export function JobEndNotice({ jobId, state, className }: { jobId: string; state: JobState | undefined; className?: string }) {
  const error = useJobsStore((s) => (jobId ? s.jobs[jobId]?.summary.error : undefined));
  if (!state || !ENDED.includes(state)) return null;
  const lines = errorLines(error?.detail);
  return (
    <Notice tone={state === 'cancelled' ? 'warning' : 'danger'} title={cs(k(`ended.${state}`))} className={className}>
      <div className="flex flex-col gap-0.5" data-testid="job-end" data-state={state}>
        {error ? (
          <div className="flex flex-wrap items-center gap-1.5">
            <CodeChip>{error.code}</CodeChip>
            <span>{error.message}</span>
          </div>
        ) : (
          <div className="text-fg-secondary">{cs('endedNoReason')}</div>
        )}
        {lines.length > 0 && (
          <ul className="flex flex-col gap-0.5" aria-label={cs('blockers')}>
            {lines.map((l, i) => {
              const title = l.code ? issueTitle({ code: l.code, message: l.text }) : l.text;
              return (
                <li key={i} className="flex flex-wrap items-center gap-1.5 text-sm" data-code={l.code}>
                  <span className="font-medium">{title}</span>
                  {l.code && <CodeChip>{l.code}</CodeChip>}
                  {l.code && title !== l.text && <span className="text-fg-secondary">{l.text}</span>}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </Notice>
  );
}
