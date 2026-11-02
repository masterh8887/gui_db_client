'use client';

// Preflight sheet (DESIGN.md §5.I, PreflightSheet): the report of `export_preflight` for a run
// (`kind: 'run'`) or of a warned job (`kind: 'resume'`). Blockers cannot be acknowledged; every
// warning must be ticked before [시작 | 계속] enables. Estimates (rows are estimates) and the
// server sessions follow. Esc / scrim / 취소 close without starting (the job, if any, stays
// warned); while the start request is in flight the sheet cannot be dismissed. Reports that
// arrive while a sheet is open wait in `extract.preflightQueue`. A resume sheet follows the job:
// once it leaves `warned`, it says so and only 닫기 remains. Nothing here changes job state.

import type { PreflightIssue, PreflightReport } from 'ipc-types';
import { CircleX, TriangleAlert } from 'lucide-react';
import { useId, useState, type ReactNode } from 'react';
import { connTag } from '@/lib/env';
import { formatBytes, formatDuration, formatInt } from '@/lib/format';
import { stateLabel, t } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { Button, Checkbox, EnvTag, Notice, Sheet, Tooltip } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore, type PreflightDialogState } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { Caption, CodeChip } from './parts';
import { qs } from './strings';

function IssueRow({ issue, ack }: { issue: PreflightIssue; ack?: { id: string; checked: boolean; onChange: (v: boolean) => void } }) {
  const danger = issue.severity === 'danger';
  const Icon = danger ? CircleX : TriangleAlert;
  return (
    <li className="flex items-start gap-2 border-t border-separator py-2 first:border-t-0" data-code={issue.code}>
      <Icon size={14} strokeWidth={1.75} className={cn('mt-0.5 shrink-0', danger ? 'text-danger' : issue.severity === 'warning' ? 'text-warning' : 'text-accent')} />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <CodeChip>{issue.code}</CodeChip>
          <span className="text-sm text-fg-secondary">{t(`severity.${issue.severity}`)}</span>
        </div>
        <div className={cn('mt-1', danger && !ack && 'text-danger')}>{issue.message}</div>
        {issue.detail && (
          <div className="text-sm text-fg-secondary" data-selectable>
            {issue.detail}
          </div>
        )}
        {issue.recommendation && (
          <div className="text-sm">
            <span className="text-fg-secondary">{qs('recommendation')}:</span> {issue.recommendation}
          </div>
        )}
        {ack && (
          <label htmlFor={ack.id} className="mt-1.5 flex w-fit items-center gap-2 text-base">
            <Checkbox id={ack.id} checked={ack.checked} onChange={ack.onChange} aria-label={qs('acknowledgeAria', { code: issue.code })} />
            {qs('acknowledge')}
          </label>
        )}
      </div>
    </li>
  );
}

function Estimates({ report }: { report: PreflightReport }) {
  const e = report.estimates;
  const rows: [string, ReactNode][] = [
    [qs('estRows'), e.rows === undefined ? '—' : `≈ ${formatInt(e.rows)} (${t('estimated')})`],
    [qs('estBytes'), formatBytes(e.bytes)],
    [
      qs('outputPath'),
      <span key="path" data-selectable className="break-all">
        {e.outputPath}
        {e.files && e.files > 1 ? <span className="text-fg-secondary"> (+{e.files - 1})</span> : null}
      </span>,
    ],
    [qs('diskFree'), formatBytes(e.diskFreeBytes)],
  ];
  if (e.workMem) {
    rows.push([
      'work_mem',
      <span key="wm">
        <span className="font-mono text-sm">{e.workMem}</span>
        {e.recommendedWorkMem && (
          <span className="text-fg-secondary">
            {' '}
            ({qs('recommendation')} <span className="font-mono text-sm">{e.recommendedWorkMem}</span>)
          </span>
        )}
      </span>,
    ]);
  }
  if (e.serverRamBytes !== undefined) rows.push([qs('serverRam'), formatBytes(e.serverRamBytes)]);
  if (report.server) rows.push([qs('server'), `${report.server.driver} ${report.server.version}`]);
  return (
    <dl className="grid grid-cols-[120px_minmax(0,1fr)] gap-x-3 gap-y-1">
      {rows.map(([k, v]) => (
        <div key={k} className="contents">
          <dt className="text-right text-fg-secondary">{k}</dt>
          <dd className="min-w-0 tabular-nums">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

function Sessions({ report }: { report: PreflightReport }) {
  if (report.sessions.length === 0) return null;
  const th = 'h-(--h-row-sm) px-2 text-left text-sm font-normal text-fg-secondary';
  return (
    <section className="flex flex-col gap-1">
      <Caption>{qs('sessions')}</Caption>
      <table className="w-full table-fixed border-collapse">
        <thead className="bg-panel">
          <tr>
            <th className={cn(th, 'w-16')}>{qs('colPid')}</th>
            <th className={th}>{qs('colUser')}</th>
            <th className={th}>{qs('colApp')}</th>
            <th className={th}>{qs('colState')}</th>
            <th className={th}>{qs('colWait')}</th>
            <th className={cn(th, 'w-20 text-right')}>{qs('colElapsed')}</th>
          </tr>
        </thead>
        <tbody>
          {report.sessions.map((s) => {
            const row = (
              <tr
                key={s.pid}
                data-blocking={s.blocking || undefined}
                className={cn('h-(--h-row-sm) border-t border-separator text-sm', s.blocking && 'bg-danger-soft text-danger')}
              >
                <td className="truncate px-2 tabular-nums">{s.pid}</td>
                <td className="truncate px-2">{s.user ?? ''}</td>
                <td className="truncate px-2">{s.application ?? ''}</td>
                <td className="truncate px-2">
                  {s.state ?? ''}
                  {s.blocking && <span className="sr-only"> ({qs('blocking')})</span>}
                </td>
                <td className="truncate px-2">{s.waitEvent ?? ''}</td>
                <td className="px-2 text-right tabular-nums">{formatDuration(s.elapsedMs)}</td>
              </tr>
            );
            return s.query ? (
              <Tooltip key={s.pid} content={<span className="font-mono">{s.query}</span>}>
                {row}
              </Tooltip>
            ) : (
              row
            );
          })}
        </tbody>
      </table>
    </section>
  );
}

/** Body + footer of one sheet. Acknowledgements belong to one report and never leak to the next. */
function PreflightContent({ dialog, queued }: { dialog: PreflightDialogState; queued: number }) {
  const uid = useId();
  const report = dialog.report;
  const none = () => report.warnings.map(() => false);
  const [ackState, setAckState] = useState<{ report: PreflightReport; acked: boolean[] }>(() => ({ report, acked: none() }));
  const acked = ackState.report === report ? ackState.acked : none();
  const setAck = (i: number, v: boolean) =>
    setAckState((st) => ({ report, acked: (st.report === report ? st.acked : none()).map((x, j) => (j === i ? v : x)) }));
  const connId = useJobsStore((s) => (dialog.kind === 'resume' ? s.jobs[dialog.jobId]?.summary.connId : dialog.req.connId));
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === connId));
  const jobConnName = useJobsStore((s) => (dialog.kind === 'resume' ? s.jobs[dialog.jobId]?.summary.connName : undefined));
  const jobState = useJobsStore((s) => (dialog.kind === 'resume' ? s.jobs[dialog.jobId]?.summary.state : undefined));
  const ex = useExtractStore.getState;

  const blocked = report.blockers.length > 0;
  const allAcked = acked.every(Boolean);
  // A resume sheet follows the job: once it is no longer warned there is nothing to continue.
  const moved = dialog.kind === 'resume' && jobState !== undefined && jobState !== 'warned';
  const canStart = !blocked && allAcked && !moved;
  const confirmLabel = dialog.kind === 'resume' ? qs('resume') : qs('start');
  const reason = blocked ? qs('blocked') : !allAcked ? qs('acknowledgeAll') : undefined;
  const codes = [...new Set(report.warnings.map((w) => w.code))];
  const close = () => ex().closeDialog();

  const title = (
    <span className="flex min-w-0 items-center gap-2">
      <span>{qs('preflightTitle')}</span>
      {conn && <EnvTag tag={connTag(conn)} variant="chip" />}
      {(conn?.name ?? jobConnName) && <span className="min-w-0 truncate text-base font-normal text-fg-secondary">{conn?.name ?? jobConnName}</span>}
    </span>
  );

  const footerStart = (
    <span className="text-sm tabular-nums text-fg-tertiary">
      {qs('checkedIn', { t: formatDuration(report.elapsedMs) })}
      {queued > 0 && <span className="text-fg-secondary"> · {qs('queued', { n: queued })}</span>}
    </span>
  );

  const reasonId = `${uid}-reason`;
  const footer = moved ? (
    <Button variant="secondary" onClick={close}>
      {qs('close')}
    </Button>
  ) : (
    <>
      <Button variant="secondary" disabled={dialog.busy} onClick={close}>
        {qs('cancel')}
      </Button>
      {/* A disabled button gets no pointer events: the reason hangs on a wrapper that is always
          rendered, so the button keeps its identity (and focus) when it enables. */}
      <span className="inline-flex" title={dialog.busy ? undefined : reason}>
        <Button
          variant="primary"
          disabled={!canStart}
          loading={dialog.busy}
          aria-describedby={reason ? reasonId : undefined}
          onClick={() => void ex().startAcknowledged(codes)}
        >
          {confirmLabel}
        </Button>
      </span>
      {reason && (
        <span id={reasonId} className="sr-only">
          {reason}
        </span>
      )}
    </>
  );

  return (
    <Sheet
      open
      onOpenChange={(o) => {
        if (!o) close();
      }}
      width="lg"
      dismissible={!dialog.busy}
      title={title}
      description={qs('preflightCounts', { b: report.blockers.length, w: report.warnings.length })}
      footerStart={footerStart}
      footer={footer}
    >
      <div className="flex flex-col gap-4">
        {dialog.error && <Notice tone="danger">{dialog.error}</Notice>}
        {moved && jobState && <Notice tone="info">{qs('stateChanged', { state: stateLabel(jobState) })}</Notice>}
        {blocked && (
          <section className="flex flex-col">
            <Caption tone="danger">{qs('blockers')}</Caption>
            <ul className="flex flex-col" aria-label={qs('blockers')}>
              {report.blockers.map((b, i) => (
                <IssueRow key={`${b.code}-${i}`} issue={b} />
              ))}
            </ul>
          </section>
        )}
        {report.warnings.length > 0 && (
          <section className="flex flex-col">
            <Caption tone="warning">{qs('warnings')}</Caption>
            <ul className="flex flex-col" aria-label={qs('warnings')}>
              {report.warnings.map((w, i) => (
                <IssueRow
                  key={`${w.code}-${i}`}
                  issue={w}
                  ack={{
                    id: `${uid}-ack-${i}`,
                    checked: !!acked[i],
                    onChange: (v) => setAck(i, v),
                  }}
                />
              ))}
            </ul>
            {!allAcked && !blocked && !moved && <div className="text-sm text-fg-secondary">{qs('acknowledgeAll')}</div>}
          </section>
        )}
        {!blocked && report.warnings.length === 0 && <div className="text-fg-secondary">{qs('noIssues')}</div>}
        <section className="flex flex-col gap-1.5">
          <Caption>{qs('estimates')}</Caption>
          <Estimates report={report} />
        </section>
        <Sessions report={report} />
      </div>
    </Sheet>
  );
}

export function PreflightSheet() {
  const dialog = useExtractStore((s) => s.preflightDialog);
  const queued = useExtractStore((s) => s.preflightQueue.length);
  if (!dialog) return null;
  return <PreflightContent dialog={dialog} queued={queued} />;
}
