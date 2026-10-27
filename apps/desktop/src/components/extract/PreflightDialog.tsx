'use client';

// Lists blockers and warnings from a `PreflightReport`. Every warning must be acknowledged
// before the extraction starts (or a warned job resumes); blockers cannot be acknowledged.

import type { PreflightIssue, PreflightReport } from 'ipc-types';
import { useMemo, useState } from 'react';
import { formatBytes, formatDuration, formatInt } from '@/lib/format';
import { t } from '@/lib/i18n';
import { Badge, Button, Checkbox, Dialog, Notice } from '@/components/ui';
import { useExtractStore } from '@/stores/extract';

const severityTone = { info: 'info', warning: 'warning', danger: 'danger' } as const;

function IssueRow({ issue, checked, onCheck }: { issue: PreflightIssue; checked?: boolean; onCheck?: (v: boolean) => void }) {
  const id = `ack-${issue.code}`;
  return (
    <Notice tone={severityTone[issue.severity]} title={<span className="flex items-center gap-2"><Badge tone="outline">{issue.code}</Badge>{t(`severity.${issue.severity}`)}</span>}>
      <div>{issue.message}</div>
      {issue.detail && <div className="text-xs text-muted-foreground">{issue.detail}</div>}
      {issue.recommendation && (
        <div className="text-xs">
          <span className="font-medium">{t('extract.recommendation')}:</span> {issue.recommendation}
        </div>
      )}
      {onCheck && (
        <label htmlFor={id} className="mt-1 flex cursor-pointer items-center gap-2 text-xs">
          <Checkbox id={id} checked={!!checked} onChange={onCheck} aria-label={`${t('extract.acknowledge')} ${issue.code}`} />
          {t('extract.acknowledge')}
        </label>
      )}
    </Notice>
  );
}

function Estimates({ report }: { report: PreflightReport }) {
  const e = report.estimates;
  const rows: [string, string][] = [
    [t('extract.planRows'), `${formatInt(e.rows)} (${t('estimated')})`],
    [t('extract.planBytes'), formatBytes(e.bytes)],
    [t('extract.outputPath'), e.outputPath + (e.files && e.files > 1 ? ` (+${e.files - 1})` : '')],
    [t('extract.diskFree'), formatBytes(e.diskFreeBytes)],
  ];
  if (e.workMem) rows.push(['work_mem', `${e.workMem}${e.recommendedWorkMem ? ` (${t('extract.recommendation')}: ${e.recommendedWorkMem})` : ''}`]);
  if (e.serverRamBytes !== undefined) rows.push([t('extract.serverRam'), formatBytes(e.serverRamBytes)]);
  if (report.server) rows.push([t('monitor.server'), `${report.server.driver} ${report.server.version}`]);
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
      {rows.map(([k, v]) => (
        <div key={k} className="contents">
          <dt className="text-muted-foreground">{k}</dt>
          <dd className="break-all tabular-nums">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function PreflightDialog() {
  const dialog = useExtractStore((s) => s.preflightDialog);
  const busy = useExtractStore((s) => s.busy);
  const error = useExtractStore((s) => s.error);
  const closeDialog = useExtractStore((s) => s.closeDialog);
  const startAcknowledged = useExtractStore((s) => s.startAcknowledged);
  const [acked, setAcked] = useState<Record<string, boolean>>({});
  // Reset the checkboxes whenever a different report is shown (derived-state pattern).
  const [prevDialog, setPrevDialog] = useState(dialog);
  if (prevDialog !== dialog) {
    setPrevDialog(dialog);
    setAcked({});
  }

  const report = dialog?.report;

  const allAcked = useMemo(() => (report ? report.warnings.every((w) => acked[w.code]) : false), [report, acked]);
  if (!dialog || !report) return null;
  const blocked = report.blockers.length > 0;
  const confirmLabel = dialog.kind === 'resume' ? t('extract.resume') : t('extract.startAnyway');

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && closeDialog()}
      title={t('extract.preflightTitle')}
      width="lg"
      // While the start request is in flight the dialog stays (Esc / outside clicks ignored).
      modal={busy === 'starting'}
      footer={
        <>
          <Button variant="ghost" onClick={closeDialog}>
            {t('common.cancel')}
          </Button>
          <Button
            variant="primary"
            disabled={blocked || !allAcked}
            loading={busy === 'starting'}
            onClick={() => void startAcknowledged(report.warnings.map((w) => w.code))}
            title={blocked ? t('extract.blocked') : !allAcked ? t('extract.acknowledgeAll') : undefined}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {error && <Notice tone="danger">{error}</Notice>}
        {blocked && (
          <section className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold text-danger">{t('extract.blockers')}</h3>
            {report.blockers.map((b, i) => (
              <IssueRow key={`${b.code}-${i}`} issue={b} />
            ))}
          </section>
        )}
        {report.warnings.length > 0 && (
          <section className="flex flex-col gap-2">
            <h3 className="text-xs font-semibold text-warning">{t('extract.warnings')}</h3>
            {report.warnings.map((w, i) => (
              <IssueRow key={`${w.code}-${i}`} issue={w} checked={acked[w.code]} onCheck={(v) => setAcked((a) => ({ ...a, [w.code]: v }))} />
            ))}
            {!allAcked && !blocked && <div className="text-xs text-muted-foreground">{t('extract.acknowledgeAll')}</div>}
          </section>
        )}
        <section className="flex flex-col gap-1">
          <h3 className="text-xs font-semibold text-muted-foreground">{t('extract.estimates')}</h3>
          <Estimates report={report} />
        </section>
        {report.sessions.length > 0 && (
          <section className="flex flex-col gap-1">
            <h3 className="text-xs font-semibold text-muted-foreground">{t('extract.sessions')}</h3>
            <table className="w-full text-xs">
              <thead className="text-muted-foreground">
                <tr className="h-6 text-left">
                  <th className="font-medium">pid</th>
                  <th className="font-medium">user</th>
                  <th className="font-medium">app</th>
                  <th className="font-medium">state</th>
                  <th className="font-medium">wait</th>
                  <th className="text-right font-medium">{t('monitor.elapsed')}</th>
                </tr>
              </thead>
              <tbody>
                {report.sessions.map((s) => (
                  <tr key={s.pid} className={`h-6 border-t border-border/60 ${s.blocking ? 'text-danger' : ''}`} title={s.query}>
                    <td className="tabular-nums">{s.pid}</td>
                    <td>{s.user ?? ''}</td>
                    <td className="truncate">{s.application ?? ''}</td>
                    <td>{s.state ?? ''}</td>
                    <td>{s.waitEvent ?? ''}</td>
                    <td className="text-right tabular-nums">{formatDuration(s.elapsedMs)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
        )}
        <div className="text-[11px] text-muted-foreground">{formatDuration(report.elapsedMs)}</div>
      </div>
    </Dialog>
  );
}
