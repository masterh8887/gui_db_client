'use client';

// The copy block of an Activity `JobRow` (kind `copy`, docs/db-copy.md 7 "Activity"): tables
// done/total, failed tables, the dry-run chip and [자세히], which reopens the wizard on this job
// (`copy.showJob`). Every window shows every job (1.1), so the counts never depend on this window
// having started or shown the job: they are the largest of `JobSummary.copy` (re-read with
// `job_get` on mount and on every state change of the job, since the jobs store keeps the summary
// it first fetched), the status this window shows and the live `copy:progress` counts.

import type { JobSummary } from 'ipc-types';
import { useEffect } from 'react';
import { Badge, Button } from '@/components/ui';
import { useCopyStore } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { copyCounts } from './model';
import { cs } from './strings';

export function CopyJobSummary({ summary }: { summary: JobSummary }) {
  const shown = useCopyStore((s) => s.runs[summary.id]?.progress);
  const live = useCopyStore((s) => s.live[summary.id]);
  const { id, state } = summary;
  useEffect(() => {
    void useJobsStore.getState().refreshJob(id);
  }, [id, state]);
  const copy = summary.copy;
  if (!copy) return null;
  const { done, total, failed } = copyCounts(copy, shown, live);
  return (
    <div className="flex items-center gap-2 pl-[22px] text-sm tabular-nums text-fg-secondary">
      <span>{cs('tables', { done, total })}</span>
      {failed > 0 && <span className="text-danger">{cs('failed', { n: failed })}</span>}
      {copy.dryRun && (
        <Badge tone="neutral" hangul>
          {cs('dryRunChip')}
        </Badge>
      )}
      <Button size="sm" variant="ghost" className="ml-auto" onClick={() => void useCopyStore.getState().showJob(summary.id)}>
        {cs('details')}
      </Button>
    </div>
  );
}
