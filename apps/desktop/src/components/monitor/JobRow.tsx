'use client';

// One job in the activity list (DESIGN.md §5.F): a flat block on the sidebar background that
// renders whatever the last `job:*` events said, for all nine states. It never infers or changes
// a state: cancel only asks (inline confirm) and sends the request, review only opens the
// preflight sheet. The warned and error strips are the only tinted elements.

import { Calculator, Copy, FileDown, FileText, FolderOpen, FolderSearch, Square, TriangleAlert, X } from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { revealPath } from '@/lib/dialogs';
import { baseName, formatByteRate, formatBytes, formatClock, formatCompact, formatDuration, formatInt, num } from '@/lib/format';
import { phaseLabel, stateLabel } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { askCancel, cancelJob, reviewWarnings } from '@/lib/job-actions';
import { jobProgressPercent } from '@/lib/job-state';
import { cn } from '@/lib/utils';
import { Button, IconButton, ProgressBar, StateChip } from '@/components/ui';
import { CopyJobSummary } from '@/components/copy/CopyJobSummary';
import { isActiveState, useJobsStore, type JobEntry } from '@/stores/jobs';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { useWindowsStore } from '@/stores/windows';
import { JobErrorStrip } from './JobErrorStrip';
import { JobTitle, jobTitle } from './JobTitle';
import { ms } from './strings';

const pad2 = (n: number) => String(n).padStart(2, '0');

/** `14:03` today, `10-26 14:03` on another day. */
export function startLabel(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  return d.toDateString() === now.toDateString() ? hm : `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${hm}`;
}

/**
 * `a · b · c` as separate spans; the line may wrap between items. Every item carries its `·` in
 * a 14px left pad and the row is shifted 14px left inside a clipping box, so the dot of an item
 * that starts a line falls outside and no line begins with `·`.
 */
function Facts({ items, className }: { items: (ReactNode | false | null | undefined)[]; className?: string }) {
  const shown = items.filter((x): x is ReactNode => x !== false && x !== null && x !== undefined && x !== '');
  if (shown.length === 0) return null;
  return (
    // Padding from `className` stays outside the clipping box (the dots must clip at the text edge).
    <div className={cn('text-sm tabular-nums text-fg-secondary', className)}>
      <div className="overflow-hidden">
        <div className="-ml-3.5 flex flex-wrap items-baseline">
          {shown.map((item, i) => (
            <span
              key={i}
              className="relative whitespace-nowrap pl-3.5 before:absolute before:left-[5px] before:text-fg-tertiary before:content-['·']"
            >
              {item}
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}

function KindIcon({ kind }: { kind: string }) {
  const Icon = kind === 'exact_count' ? Calculator : kind === 'local_open' ? FolderSearch : kind === 'copy' ? Copy : FileDown;
  return <Icon size={14} strokeWidth={1.75} aria-hidden className="shrink-0 text-fg-secondary" />;
}

async function reveal(path: string): Promise<void> {
  try {
    await revealPath(path);
  } catch (e) {
    logLocal('error', `reveal_path: ${errorText(e)}`);
  }
}

export interface JobRowProps {
  entry: JobEntry;
  /** `inspector` also shows the server line (state, wait, bottleneck, temp files, pid). */
  variant: 'inspector' | 'popover';
}

export function JobRow({ entry, variant }: JobRowProps) {
  const { summary, progress, server } = entry;
  const { id, state } = summary;
  const active = isActiveState(state);
  // A confirmation left over from before the job ended never lingers on its result.
  const confirming = useUiStore((s) => s.cancelConfirmJobId === id) && active;
  const remove = useJobsStore((s) => s.remove);
  // Jobs are process-global: with several windows, say which ones another window started.
  const manyWindows = useWindowsStore((s) => s.windows.length > 1);
  const local = useJobsStore((s) => s.local[id] === true);
  const fromOtherWindow = manyWindows && !local;
  const [reviewing, setReviewing] = useState(false);
  const noRef = useRef<HTMLButtonElement>(null);
  const focusNo = useRef(false);

  // The confirmation was asked for from this row's button: put the keyboard on [아니요]
  // (Esc and the global shortcut never move focus).
  useEffect(() => {
    if (confirming && focusNo.current) noRef.current?.focus();
    focusNo.current = false;
  }, [confirming]);

  const elapsedMs = num(progress?.elapsedMs) ?? num(server?.elapsedMs);
  const eta = num(progress?.etaSec);
  const percent = state === 'running' ? jobProgressPercent(progress) : undefined;
  const result = summary.result;
  const warnings = summary.preflight?.warnings.length;

  const review = async () => {
    setReviewing(true);
    try {
      await reviewWarnings(id);
    } finally {
      setReviewing(false);
    }
  };

  const kindLabel =
    summary.kind === 'exact_count'
      ? ms('kindCount')
      : summary.kind === 'local_open'
        ? ms('kindOpen')
        : summary.kind === 'copy'
          ? ms('kindCopy')
          : ms('kindExport');

  return (
    <article
      data-state={state}
      aria-label={`${jobTitle(summary)} ${stateLabel(state)}`}
      className="group relative flex flex-col gap-1.5 px-3 py-2.5 hover:bg-hover"
    >
      <header className="flex min-w-0 items-center gap-2">
        <span title={kindLabel} className="inline-flex">
          <KindIcon kind={summary.kind} />
        </span>
        <JobTitle summary={summary} />
        <StateChip state={state} />
        {active ? (
          !confirming && (
            <IconButton
              label={ms('cancel')}
              size="sm"
              className="opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 focus-visible:opacity-100"
              onClick={() => {
                focusNo.current = true;
                askCancel(id);
              }}
            >
              <Square size={12} strokeWidth={2} />
            </IconButton>
          )
        ) : (
          <IconButton
            label={ms('remove')}
            size="sm"
            className="opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 focus-visible:opacity-100"
            onClick={() => remove(id)}
          >
            <X size={14} />
          </IconButton>
        )}
      </header>
      <div className="-mt-1 truncate pl-[22px] text-sm text-fg-secondary">
        {summary.connName ? ms('connStart', { conn: summary.connName, time: startLabel(summary.startedAt) }) : startLabel(summary.startedAt)}
      </div>
      {fromOtherWindow && <div className="-mt-1 truncate pl-[22px] text-xs text-fg-tertiary">{ms('otherWindow')}</div>}
      {summary.kind === 'copy' && <CopyJobSummary summary={summary} />}

      {confirming && (
        <div
          role="alertdialog"
          aria-label={ms('cancelConfirm')}
          className="flex items-center gap-2 pl-[22px] text-sm"
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault();
              e.stopPropagation();
              useUiStore.getState().setCancelConfirm(null);
            }
          }}
        >
          <span className="min-w-0 flex-1">{ms('cancelConfirm')}</span>
          <Button size="sm" variant="dangerFilled" onClick={() => void cancelJob(id)}>
            {ms('yes')}
          </Button>
          <Button ref={noRef} size="sm" variant="secondary" onClick={() => useUiStore.getState().setCancelConfirm(null)}>
            {ms('no')}
          </Button>
        </div>
      )}

      {state === 'warned' && (
        <div className="ml-[22px] flex items-center gap-2 rounded-md bg-warning-soft py-1 pr-1 pl-2 text-sm">
          <TriangleAlert size={14} strokeWidth={1.75} aria-hidden className="shrink-0 text-warning" />
          <span className="min-w-0 flex-1">{warnings !== undefined ? ms('warned', { n: warnings }) : ms('warnedUnknown')}</span>
          <Button size="sm" variant="secondary" loading={reviewing} onClick={() => void review()}>
            {ms('review')}
          </Button>
        </div>
      )}

      {(state === 'running' || state === 'finishing') && (
        <div className="flex flex-col gap-1.5 pl-[22px]">
          <ProgressBar
            size="sm"
            value={percent}
            indeterminate={state === 'finishing' || percent === undefined}
            aria-label={`${jobTitle(summary)} ${stateLabel(state)}`}
          />
          <Facts
            items={[
              progress && ms('rows', { n: formatCompact(progress.rows) }),
              progress && formatBytes(progress.bytesOut),
              progress && formatByteRate(progress.rateBytesPerSec),
              progress && ms('rowRate', { n: formatCompact(progress.rateRowsPerSec) }),
              elapsedMs !== undefined && ms('elapsed', { t: formatClock(elapsedMs / 1000) }),
              state === 'running' && eta !== undefined && ms('eta', { t: formatClock(eta) }),
              state === 'running' && progress && phaseLabel(progress.phase),
              progress && progress.files > 1 && ms('files', { n: progress.files }),
            ]}
          />
        </div>
      )}

      {active && server && variant === 'inspector' && (
        <Facts
          className="pl-[22px]"
          items={[
            server.state && ms('server', { state: server.state }),
            server.waitEvent && ms('wait', { event: server.waitEvent }),
            server.bottleneck &&
              ms('bottleneck', { b: server.bottleneck === 'network' ? ms('bottleneckNetwork') : ms('bottleneckServer') }),
            (num(server.tempBytes) ?? 0) > 0 && ms('tempFiles', { size: formatBytes(server.tempBytes) }),
            server.pid && ms('pid', { pid: server.pid }),
          ]}
        />
      )}

      {!active && (
        <div className="flex flex-col gap-1.5 pl-[22px]">
          {summary.error && <JobErrorStrip error={summary.error} state={state} />}
          {summary.exactCount && (
            <Facts
              className="text-fg"
              items={[ms('exactCount', { rows: formatInt(summary.exactCount.rows) }), formatDuration(summary.exactCount.elapsedMs)]}
            />
          )}
          {result && (
            <Facts
              items={[
                ms('rows', { n: formatInt(result.rows) }),
                formatBytes(result.bytes),
                formatDuration(result.durationMs),
                result.verifiedRows !== undefined && ms('verified', { n: formatInt(result.verifiedRows) }),
              ]}
            />
          )}
          {result && result.files.length > 0 && (
            <ul className="flex flex-col">
              {result.files.map((f) => (
                <li key={f} className="flex h-(--h-control-sm) min-w-0 items-center gap-1.5">
                  <FileText size={14} strokeWidth={1.75} aria-hidden className="shrink-0 text-fg-tertiary" />
                  <button
                    type="button"
                    title={f}
                    data-selectable
                    onClick={() => void reveal(f)}
                    className="min-w-0 flex-1 truncate text-left text-base text-fg outline-none hover:text-accent focus-visible:underline"
                  >
                    {baseName(f)}
                  </button>
                  <IconButton label={`${ms('openFolder')}: ${baseName(f)}`} size="sm" onClick={() => void reveal(f)}>
                    <FolderOpen size={14} strokeWidth={1.75} />
                  </IconButton>
                </li>
              ))}
            </ul>
          )}
          {result && result.notes.length > 0 && (
            <ul className="flex flex-col gap-0.5 text-sm text-fg-secondary">
              {result.notes.map((n, i) => (
                <li key={i}>{n}</li>
              ))}
            </ul>
          )}
        </div>
      )}
    </article>
  );
}
