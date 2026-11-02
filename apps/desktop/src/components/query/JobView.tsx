'use client';

// Export panel 작업 (DESIGN.md §5.D): the doc's last export job as the `job:*` events report it:
// state, progress, rates, server wait / bottleneck, files, error, notes. In `warned` a strip with
// [검토] opens the resume sheet (it only opens the sheet). Read-only otherwise: Rust is the state
// of record and nothing here changes the job.

import type { JobProgress, JobServer, JobSummary } from 'ipc-types';
import { FolderOpen, TriangleAlert } from 'lucide-react';
import { revealPath } from '@/lib/dialogs';
import { baseName, formatBytes, formatByteRate, formatClock, formatCompact, formatDuration, formatInt } from '@/lib/format';
import { phaseLabel } from '@/lib/i18n';
import { reviewWarnings, showActivity } from '@/lib/job-actions';
import { jobProgressPercent } from '@/lib/job-state';
import { Button, IconButton, ProgressBar, StateChip } from '@/components/ui';
import { useJobsStore } from '@/stores/jobs';
import { logLocal } from '@/stores/logs';
import { useQueryDoc } from '@/stores/extract';
import { CodeChip, PanelEmpty } from './parts';
import { parseUnmappableNote } from './jobNotes';
import { qs } from './strings';

/** `412K행/s` / `412K rows/s`. */
function rowRate(n: number): string {
  return `${formatCompact(n)}${qs('rowsUnit')}/s`;
}

function Dot() {
  return <span aria-hidden className="text-fg-tertiary"> · </span>;
}

/** `19.8M행 · 1.1 GB · 98 MB/s · 412K행/s · 경과 4:51 · 남은 시간 3:12 · 쓰는 중`. */
function ProgressLine({ p, finishing }: { p: JobProgress; finishing: boolean }) {
  const parts = [
    `${formatCompact(p.rows)}${qs('rowsUnit')}`,
    formatBytes(p.bytesOut),
    formatByteRate(p.rateBytesPerSec),
    rowRate(p.rateRowsPerSec),
    qs('elapsed', { t: formatClock(Number(p.elapsedMs) / 1000) }),
  ];
  if (!finishing && p.etaSec !== undefined) parts.push(qs('eta', { t: formatClock(p.etaSec) }));
  parts.push(phaseLabel(p.phase));
  if (p.files > 1) parts.push(`${qs('files')} ${p.files}`);
  return (
    <div className="text-sm tabular-nums text-fg-secondary" data-testid="job-progress">
      {parts.map((s, i) => (
        <span key={i}>
          {i > 0 && <Dot />}
          {s}
        </span>
      ))}
    </div>
  );
}

function ServerLine({ s }: { s: JobServer }) {
  const parts: string[] = [];
  if (s.state) parts.push(qs('serverState', { state: s.state }));
  if (s.waitEvent) parts.push(qs('serverWait', { event: s.waitEvent }));
  if (s.bottleneck) parts.push(qs('bottleneck', { b: s.bottleneck === 'network' ? qs('bottleneckNetwork') : qs('bottleneckServer') }));
  if (s.tempBytes !== undefined && Number(s.tempBytes) > 0) parts.push(qs('tempFiles', { size: formatBytes(s.tempBytes) }));
  if (s.pid) parts.push(`pid ${s.pid}`);
  if (parts.length === 0) return null;
  return <div className="text-sm text-fg-secondary">{parts.join(' · ')}</div>;
}

function Files({ files }: { files: readonly string[] }) {
  if (files.length === 0) return null;
  const reveal = (path: string) =>
    void revealPath(path).catch((e: unknown) => logLocal('error', String(e instanceof Error ? e.message : e)));
  return (
    <div className="flex flex-col">
      <div className="text-sm font-semibold text-fg-secondary">{qs('files')}</div>
      <ul className="flex flex-col">
        {files.map((f) => (
          <li key={f} className="group flex h-(--h-row) items-center gap-2">
            <span className="min-w-0 truncate" title={f} data-selectable>
              {baseName(f)}
            </span>
            <IconButton label={qs('revealFile')} size="sm" onClick={() => reveal(f)} className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100">
              <FolderOpen size={14} strokeWidth={1.75} />
            </IconButton>
          </li>
        ))}
      </ul>
    </div>
  );
}

function Terminal({ job }: { job: JobSummary }) {
  const r = job.result;
  return (
    <>
      {job.error && (
        <div
          role="alert"
          className={`flex flex-col gap-0.5 rounded-md px-2.5 py-1.5 ${job.state === 'cancelled' ? 'bg-warning-soft' : 'bg-danger-soft'}`}
        >
          <div className="flex items-center gap-2">
            <CodeChip>{job.error.code}</CodeChip>
            <span className="min-w-0 break-words">{job.error.message}</span>
          </div>
          {job.error.detail && (
            <div className="text-sm text-fg-secondary" data-selectable>
              {job.error.detail}
            </div>
          )}
        </div>
      )}
      {job.exactCount && <div className="tabular-nums">{qs('exactRows', { rows: formatInt(job.exactCount.rows) })}</div>}
      {r && (
        <div className="text-sm tabular-nums text-fg-secondary">
          {formatInt(r.rows)}
          {qs('rowsUnit')}
          <Dot />
          {formatBytes(r.bytes)}
          <Dot />
          {formatDuration(r.durationMs)}
          {r.verifiedRows !== undefined && (
            <>
              <Dot />
              {qs('verified', { rows: formatInt(r.verifiedRows) })}
            </>
          )}
        </div>
      )}
      {r && <Files files={r.files} />}
      {r && r.notes.length > 0 && (
        <div className="flex flex-col gap-0.5">
          <div className="text-sm font-semibold text-fg-secondary">{qs('notes')}</div>
          {r.notes.map((n, i) => (
            <JobNote key={i} note={n} />
          ))}
        </div>
      )}
    </>
  );
}

export function JobView({ tabId }: { tabId: string }) {
  const jobId = useQueryDoc(tabId, (d) => d.lastJobId) ?? null;
  const entry = useJobsStore((s) => (jobId ? s.jobs[jobId] : undefined));

  if (!jobId) return <PanelEmpty title={qs('jobEmpty')} hint={qs('jobEmptyHint')} />;
  if (!entry) return <PanelEmpty title={qs('jobGone')} />;

  const job = entry.summary;
  const progress = entry.progress ?? job.progress;
  const server = entry.server ?? job.server;
  const state = job.state;
  const running = state === 'running' || state === 'finishing';
  const terminal = state === 'done' || state === 'failed' || state === 'cancelled' || state === 'rejected';
  const pct = state === 'running' ? jobProgressPercent(progress) : undefined;
  const warnCount = job.preflight?.warnings.length;

  return (
    <section className="flex max-w-[880px] flex-col gap-2.5" aria-label={qs('segJob')} data-job-id={jobId}>
      <div className="flex items-center gap-2">
        <StateChip state={state} />
        <span className="min-w-0 flex-1 truncate font-medium" title={job.label}>
          {job.label}
        </span>
        <Button size="sm" variant="link" onClick={showActivity}>
          {qs('allActivity')}
        </Button>
      </div>

      {state === 'warned' && (
        <div role="alert" className="flex items-center gap-2 rounded-md bg-warning-soft px-2.5 py-1.5">
          <TriangleAlert size={14} strokeWidth={1.75} className="shrink-0 text-warning" />
          <span className="min-w-0 flex-1">{qs('warnedStrip', { n: warnCount ?? '?' })}</span>
          <Button size="sm" variant="secondary" onClick={() => void reviewWarnings(jobId)}>
            {qs('review')}
          </Button>
        </div>
      )}

      {(running || state === 'pending' || state === 'preflight') && (
        <ProgressBar
          aria-label={qs('segJob')}
          value={pct}
          indeterminate={pct === undefined}
          size="sm"
        />
      )}
      {running && progress && <ProgressLine p={progress} finishing={state === 'finishing'} />}
      {running && server && <ServerLine s={server} />}
      {terminal && <Terminal job={job} />}
    </section>
  );
}

/**
 * One `JobResult` note. The replaced-characters note of an EUC-KR CSV (core
 * `encoding::unmappable_note`) is localized and shown as a warning: the file is complete, but
 * some characters became `?`.
 */
function JobNote({ note }: { note: string }) {
  const u = parseUnmappableNote(note);
  if (!u) return <div className="text-sm text-fg-secondary">{note}</div>;
  return (
    <div className="flex items-start gap-1 text-sm text-warning" data-note="unmappable" title={note}>
      <TriangleAlert size={12} strokeWidth={1.75} className="mt-0.5 shrink-0" />
      <span className="min-w-0">{qs('unmappableNote', { count: formatInt(u.count), encoding: u.encoding })}</span>
    </div>
  );
}
