'use client';

// One job as the monitor shows it. Renders whatever the last `job:*` events said; never
// infers state. Covers all nine states.

import { AlertTriangle, Calculator, FileDown, FolderOpen, Square, X } from 'lucide-react';
import { useState } from 'react';
import { revealPath } from '@/lib/dialogs';
import { baseName, formatByteRate, formatBytes, formatDuration, formatEta, formatInt, formatRowRate, num } from '@/lib/format';
import { phaseLabel, stateLabel, t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { STATE_TONE, isBusyState } from '@/lib/job-state';
import { cn } from '@/lib/utils';
import { Badge, Button, IconButton, Notice, Spinner } from '@/components/ui';
import { useExtractStore } from '@/stores/extract';
import { isActiveState, useJobsStore, type JobEntry } from '@/stores/jobs';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

function Stat({ label, value, className }: { label: string; value: React.ReactNode; className?: string }) {
  return (
    <div className={cn('min-w-0', className)}>
      <div className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="truncate text-[13px] tabular-nums">{value}</div>
    </div>
  );
}

function ProgressBar({ percent, indeterminate }: { percent?: number; indeterminate: boolean }) {
  return (
    <div className="relative h-1.5 w-full overflow-hidden rounded bg-muted" role="progressbar" aria-valuenow={percent !== undefined ? Math.round(percent) : undefined} aria-valuemin={0} aria-valuemax={100}>
      {indeterminate ? (
        <div className="absolute inset-y-0 w-1/3 animate-[slide_1.2s_ease-in-out_infinite] rounded bg-accent/70" />
      ) : (
        <div className="h-full rounded bg-accent transition-[width]" style={{ width: `${percent ?? 0}%` }} />
      )}
    </div>
  );
}

export function JobCard({ entry, compact = false }: { entry: JobEntry; compact?: boolean }) {
  const { summary, progress, server } = entry;
  const state = summary.state;
  const active = isActiveState(state);
  // Only an active job can be cancelled: a confirmation left over from before the job ended
  // must not linger on its result card.
  const confirming = useUiStore((s) => s.cancelConfirmJobId === summary.id) && active;
  const setCancelConfirm = useUiStore((s) => s.setCancelConfirm);
  const remove = useJobsStore((s) => s.remove);
  const cancel = useJobsStore((s) => s.cancel);
  const refreshJob = useJobsStore((s) => s.refreshJob);
  const openResumeDialog = useExtractStore((s) => s.openResumeDialog);
  const [loadingReport, setLoadingReport] = useState(false);

  const elapsedMs = num(progress?.elapsedMs) ?? num(server?.elapsedMs);
  const eta = num(progress?.etaSec);
  const percent = elapsedMs !== undefined && eta !== undefined && eta >= 0 ? (elapsedMs / (elapsedMs + eta * 1000)) * 100 : undefined;
  const result = summary.result;

  // The preflight report arrives with `job_get` (AppEvents fetches it when `warned` comes in).
  // If that answer has not arrived, or was lost, fetch it here instead of leaving the job stuck
  // in `warned` with only "cancel" as a way out.
  const reviewWarnings = async () => {
    let report = summary.preflight;
    if (!report) {
      setLoadingReport(true);
      try {
        const fresh = await refreshJob(summary.id);
        report = fresh?.state === 'warned' ? fresh.preflight : undefined;
      } finally {
        setLoadingReport(false);
      }
    }
    if (report) openResumeDialog(summary.id, report);
    // Only worth saying while the job still waits; otherwise the card already shows its new state.
    else if (useJobsStore.getState().jobs[summary.id]?.summary.state === 'warned') logLocal('error', t('monitor.reportUnavailable'), summary.id);
  };

  const doCancel = async () => {
    setCancelConfirm(null);
    try {
      await cancel(summary.id);
    } catch (e) {
      logLocal('error', `job_cancel: ${errorText(e)}`, summary.id);
    }
  };

  return (
    <article
      data-state={state}
      aria-label={`${summary.label} ${stateLabel(state)}`}
      className={cn(
        'flex flex-col gap-2 rounded border bg-background p-2',
        state === 'failed' ? 'border-danger/50' : state === 'warned' ? 'border-warning/60' : 'border-border',
      )}
    >
      <header className="flex items-center gap-2">
        <span className="text-muted-foreground">{summary.kind === 'exact_count' ? <Calculator size={13} /> : <FileDown size={13} />}</span>
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium" title={summary.label}>
          {summary.label}
        </span>
        <Badge tone={STATE_TONE[state]}>
          {isBusyState(state) && <Spinner size={9} tone="current" />}
          {stateLabel(state)}
        </Badge>
        {active ? (
          <IconButton label={t('monitor.cancel')} size="sm" onClick={() => setCancelConfirm(summary.id)} disabled={confirming}>
            <Square size={11} />
          </IconButton>
        ) : (
          <IconButton label={t('monitor.remove')} size="sm" onClick={() => remove(summary.id)}>
            <X size={12} />
          </IconButton>
        )}
      </header>
      {summary.connName && <div className="-mt-1 text-[11px] text-muted-foreground">{summary.connName}</div>}

      {confirming && (
        <div role="alertdialog" className="flex items-center gap-2 rounded border border-warning/50 bg-warning/10 px-2 py-1 text-xs">
          <AlertTriangle size={12} className="text-warning" />
          <span className="flex-1">{t('monitor.cancelConfirm')}</span>
          <Button size="sm" variant="danger" onClick={() => void doCancel()}>
            {t('common.yes')}
          </Button>
          <Button size="sm" variant="ghost" onClick={() => setCancelConfirm(null)}>
            {t('common.no')}
          </Button>
        </div>
      )}

      {state === 'warned' && (
        <div className="flex items-center gap-2 text-xs">
          <span className="flex-1 text-warning">{summary.preflight ? `${summary.preflight.warnings.length} ${t('extract.warnings')}` : stateLabel(state)}</span>
          <Button size="sm" variant="primary" loading={loadingReport} disabled={loadingReport} onClick={() => void reviewWarnings()}>
            {t('monitor.acknowledge')}
          </Button>
        </div>
      )}

      {(state === 'running' || state === 'finishing') && (
        <>
          <ProgressBar percent={percent} indeterminate={percent === undefined || state === 'finishing'} />
          <div className="grid grid-cols-3 gap-x-2 gap-y-1">
            <Stat label={t('monitor.phase')} value={progress ? phaseLabel(progress.phase) : state === 'finishing' ? phaseLabel('finishing') : '—'} />
            <Stat label={t('monitor.elapsed')} value={formatDuration(elapsedMs)} />
            <Stat label={t('monitor.eta')} value={formatEta(eta)} />
            <Stat label={t('monitor.rows')} value={formatInt(progress?.rows)} />
            <Stat label={t('monitor.rate')} value={formatByteRate(progress?.rateBytesPerSec)} />
            <Stat label="" value={<span className="text-xs text-muted-foreground">{formatRowRate(progress?.rateRowsPerSec)}</span>} />
            {progress && <Stat label={t('history.bytes')} value={formatBytes(progress.bytesOut)} />}
            {progress && progress.files > 1 && <Stat label={t('monitor.files')} value={progress.files} />}
          </div>
        </>
      )}

      {active && server && !compact && (
        <div className="grid grid-cols-3 gap-x-2 gap-y-1 border-t border-border/60 pt-1">
          <Stat label={t('monitor.server')} value={server.state ?? '—'} />
          <Stat label={t('monitor.wait')} value={server.waitEvent ?? '—'} />
          <Stat
            label={t('monitor.bottleneck')}
            value={server.bottleneck ? <Badge tone={server.bottleneck === 'network' ? 'accent' : 'warning'}>{t(`bottleneck.${server.bottleneck}`)}</Badge> : '—'}
          />
          {server.tempBytes !== undefined && num(server.tempBytes)! > 0 && <Stat label={t('monitor.tempBytes')} value={formatBytes(server.tempBytes)} />}
          {server.pid && <Stat label="pid" value={server.pid} />}
        </div>
      )}

      {!active && (
        <div className="flex flex-col gap-1">
          {summary.error && (
            <Notice tone={state === 'cancelled' ? 'warning' : 'danger'} title={summary.error.code}>
              {summary.error.message}
              {summary.error.detail && <div className="text-xs text-muted-foreground">{summary.error.detail}</div>}
            </Notice>
          )}
          {summary.exactCount && (
            <div className="text-[13px]">
              {t('monitor.exactCount')}: <b className="tabular-nums">{formatInt(summary.exactCount.rows)}</b>{' '}
              <span className="text-xs text-muted-foreground">({formatDuration(summary.exactCount.elapsedMs)})</span>
            </div>
          )}
          {result && (
            <div className="grid grid-cols-3 gap-x-2 gap-y-1">
              <Stat label={t('monitor.rows')} value={formatInt(result.rows)} />
              <Stat label={t('history.bytes')} value={formatBytes(result.bytes)} />
              <Stat label={t('monitor.duration')} value={formatDuration(result.durationMs)} />
              {result.verifiedRows !== undefined && <Stat label={t('monitor.verified')} value={formatInt(result.verifiedRows)} />}
            </div>
          )}
          {result && result.files.length > 0 && (
            <ul className="flex flex-col gap-0.5">
              {result.files.map((f) => (
                <li key={f} className="flex items-center gap-1 text-xs">
                  <button type="button" className="min-w-0 flex-1 truncate text-left text-accent hover:underline" title={f} onClick={() => void revealPath(f)}>
                    {baseName(f)}
                  </button>
                  <IconButton label={f} size="sm" onClick={() => void revealPath(f)}>
                    <FolderOpen size={11} />
                  </IconButton>
                </li>
              ))}
            </ul>
          )}
          {result && result.notes.length > 0 && !compact && <div className="text-[11px] text-muted-foreground">{result.notes.join(' · ')}</div>}
        </div>
      )}
    </article>
  );
}
