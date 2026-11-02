'use client';

// The 결과 view (docs/db-copy.md 7): summary strip (완료 / 실패 / 건너뜀, duration, 검증), the
// done-with-problems banner, the skipped-existing sentence, the dry-run header with [실제로 복사],
// the verification table (with value fixes per column and warning samples), runtime warnings,
// what stays in the target (`left_behind`), and [이어서 실행] (asks for `partial` when tables were
// appended into, and for the production name), [쓰기 허용 해제], [계획 다시 보기]. When the job
// shown was rejected or failed without a table error (e.g. a resume rejected by its blockers,
// whose run result is shown here), the job's error says why (`JobEndNotice`).

import type { CopyResult, CopyStatus, CopyTableResult, PlanItem, ResumePartial, ValueFix } from 'ipc-types';
import { useMemo, useState, type ReactNode } from 'react';
import { Button, Input, Notice, Tooltip } from '@/components/ui';
import { formatDuration, formatInt } from '@/lib/format';
import { toast } from '@/lib/toast';
import { cn } from '@/lib/utils';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { JobEndNotice } from './JobEndNotice';
import { isTerminal, issueTitle } from './model';
import { Caption, RadioList } from './parts';
import { cs, k } from './strings';

function fixText(f: ValueFix): string {
  const key = `fix.${f.code}`;
  const n = formatInt(f.count);
  return (cs.keys as readonly string[]).includes(key) ? cs(k(key), { n }) : cs('fixOther', { code: f.code, n });
}

/** "created_at: 0 날짜 → NULL 12건, 잘못된 날짜 3건 · updated_at: …" (one group per column). */
function fixesByColumn(fixes: ValueFix[]): string {
  const cols = new Map<string, string[]>();
  for (const f of fixes) cols.set(f.column, [...(cols.get(f.column) ?? []), fixText(f)]);
  return [...cols.entries()].map(([c, texts]) => `${c}: ${texts.join(', ')}`).join(' · ');
}

function VerifyCell({ t }: { t: CopyTableResult }) {
  const tone = t.verify === 'match' ? 'text-success' : t.verify === 'mismatch' || t.verify === 'error' ? 'text-danger' : t.verify === 'lossy' ? 'text-warning' : 'text-fg-tertiary';
  return <span className={cn('text-sm font-medium', tone)}>{cs(k(`verify.${t.verify}`))}</span>;
}

function VerificationTable({ result, items }: { result: CopyResult; items: Map<string, PlanItem> }) {
  const th = 'h-6 px-2 text-left text-sm font-normal text-fg-secondary';
  const loaded = result.tables.filter((t) => t.state !== 'skipped' || result.tables.length < 50);
  return (
    <section className="flex flex-col gap-1">
      <Caption>{cs('verification')}</Caption>
      <div className="max-h-[300px] overflow-auto border-b border-separator">
        <table className="w-full table-fixed border-collapse">
          <thead className="sticky top-0 bg-panel">
            <tr>
              <th className={th}>{cs('vcol.table')}</th>
              <th className={cn(th, 'w-28 text-right')}>{cs('vcol.read')}</th>
              <th className={cn(th, 'w-28 text-right')}>{cs('vcol.target')}</th>
              <th className={cn(th, 'w-28')}>{cs('vcol.result')}</th>
              <th className={cn(th, 'w-20 text-right')}>{cs('vcol.warnings')}</th>
            </tr>
          </thead>
          <tbody>
            {loaded.map((t) => {
              const item = items.get(t.itemId);
              const warn = Number(t.dataWarnings) + Number(t.unmappableChars);
              const samples = t.warningSamples.join('\n');
              const cell = (
                // Focusable with the samples as its accessible name, so keyboard and screen-reader
                // users reach them (sighted mouse users get the tooltip); plain when there are none.
                <span
                  className={cn('tabular-nums', warn > 0 ? 'text-warning' : 'text-fg-tertiary', t.warningSamples.length > 0 && 'rounded-sm outline-none focus-visible:outline-2 focus-visible:outline-focus')}
                  {...(t.warningSamples.length > 0 ? { tabIndex: 0, 'aria-label': cs('warningSamplesAria', { n: warn, samples }) } : {})}
                >
                  {warn > 0 ? formatInt(warn) : '—'}
                </span>
              );
              return (
                <tr key={t.itemId} className={cn('h-7 border-t border-separator text-base', t.state === 'failed' && 'text-danger')} data-item={t.itemId}>
                  <td className="truncate px-2 py-1" title={t.itemId}>
                    {item?.targetSchema ? `${item.targetSchema}.` : ''}
                    {t.targetName}
                    {t.valueFixes.length > 0 && <div className="truncate text-sm text-fg-secondary">{fixesByColumn(t.valueFixes)}</div>}
                    {t.error && <div className="truncate text-sm text-danger">{cs('tableError', { code: t.error.code, message: t.error.message })}</div>}
                  </td>
                  <td className="px-2 text-right text-sm tabular-nums">{formatInt(t.rowsRead)}</td>
                  <td className="px-2 text-right text-sm tabular-nums">{t.targetCount !== undefined ? formatInt(t.targetCount) : <span className="text-fg-tertiary">—</span>}</td>
                  <td className="px-2">{t.state === 'skipped' ? <span className="text-sm text-fg-secondary">{cs('state.skipped')}</span> : <VerifyCell t={t} />}</td>
                  <td className="px-2 text-right text-sm">
                    {t.warningSamples.length > 0 ? <Tooltip content={<span className="font-mono text-xs">{t.warningSamples.join('\n')}</span>}>{cell}</Tooltip> : cell}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

/**
 * Resume with the session passwords core already holds; an "ask every time" side without one is
 * asked first (DbPasswordDialog), and [이어서 실행] is pressed again afterwards.
 */
async function resumeRun(runId: string, partial: ResumePartial, confirmProduction?: string): Promise<void> {
  const { draft } = useCopyStore.getState().wizard;
  const conns = useConnectionsStore.getState();
  const locked = [draft.source.connId, draft.target.connId].find((id) => needsSessionPassword(conns, id));
  if (locked) {
    // Continue (or ask for the other locked side) once this one is unlocked, instead of making the
    // user press [이어서 실행] again for each.
    useUiStore.getState().askDbPassword(locked, 'browse', { then: () => void resumeRun(runId, partial, confirmProduction) });
    return;
  }
  await useCopyStore.getState().resume(runId, partial, confirmProduction);
}

/** The resume inputs, then one button row: [이어서 실행] (primary) followed by `children`. */
function ResumePanel({ status, result, children }: { status: CopyStatus; result: CopyResult; children?: ReactNode }) {
  const draft = useCopyStore((s) => s.wizard.draft);
  const starting = useCopyStore((s) => s.wizard.starting);
  const target = useConnectionsStore((s) => s.connections.find((c) => c.id === draft.target.connId));
  const [partial, setPartial] = useState<ResumePartial>('fail');
  const [name, setName] = useState('');
  // The run's request counts too: a stored result shown without its plan (another process, or a
  // resume that was rejected) still asks when any table was appended into.
  const appended =
    (status.plan?.items ?? []).some((i) => i.kind === 'table' && i.action === 'append') ||
    result.existing === 'append' ||
    draft.options.existing === 'append' ||
    draft.options.existingOverrides.some((o) => o.policy === 'append');
  const production = target?.tag === 'production';
  const blocked = production && name !== target?.name;
  return (
    <section className="flex flex-col gap-3" aria-label={cs('resume')}>
      {appended && (
        <div className="flex flex-col gap-1">
          <Caption>{cs('resumePartial')}</Caption>
          <RadioList
            name="partial"
            label={cs('resumePartial')}
            value={partial}
            onChange={setPartial}
            options={(['fail', 'truncate', 'skip'] as const).map((p) => ({ value: p, label: cs(k(`partial.${p}`)), danger: p === 'truncate' }))}
          />
        </div>
      )}
      {production && target && (
        // The same danger notice as the plan step's production confirmation.
        <Notice tone="danger">
          <label className="flex flex-col gap-1">
            <span>{cs('productionPrompt', { name: target.name })}</span>
            <Input aria-label={cs('productionInput')} value={name} onChange={(e) => setName(e.target.value)} className="w-80" autoComplete="off" spellCheck={false} />
          </label>
        </Notice>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex" title={blocked ? cs('reason.production') : undefined}>
          <Button size="sm" variant="primary" disabled={blocked || starting} loading={starting} onClick={() => void resumeRun(result.runId, partial, production ? name : undefined)}>
            {cs('resume')}
          </Button>
        </span>
        {children}
      </div>
    </section>
  );
}

export function ResultView({ status }: { status: CopyStatus | undefined }) {
  const draft = useCopyStore((s) => s.wizard.draft);
  const target = useConnectionsStore((s) => s.connections.find((c) => c.id === draft.target.connId));
  const jobState = useJobsStore((s) => (status ? s.jobs[status.jobId]?.summary.state : undefined)) ?? status?.state;
  const items = useMemo(() => new Map((status?.plan?.items ?? []).map((i) => [i.id, i])), [status?.plan]);
  const result = status?.result;
  if (!status || !result) {
    if (status && isTerminal(jobState)) {
      return jobState === 'done' ? <div className="text-fg-secondary">{cs('endedNoResult')}</div> : <JobEndNotice jobId={status.jobId} state={jobState} />;
    }
    return <div className="text-fg-secondary">{cs('resultMissing')}</div>;
  }

  const done = result.tables.filter((t) => t.state === 'done' || t.state === 'planned').length;
  const failed = result.tables.filter((t) => t.state === 'failed' || t.state === 'cancelled').length;
  const skipped = result.tables.filter((t) => t.state === 'skipped').length;
  const mismatches = result.tables.filter((t) => t.verify === 'mismatch' || t.verify === 'error').length;
  const verifyRun = result.tables.some((t) => t.verify !== 'not_run');
  const family = status.plan?.family ?? (target?.driver as string | undefined);
  const resumable = result.resumable && (jobState !== 'done' || result.problems > 0);
  const checksum = result.tables.some((t) => t.checksumSource !== undefined);
  const label = (it: string) => {
    const item = items.get(it);
    return item ? `${item.targetSchema ? `${item.targetSchema}.` : ''}${item.targetName}` : it;
  };

  const clearWritable = async () => {
    if (!target) return;
    if (await useCopyStore.getState().clearWriteAllowed(target.id)) toast(cs('clearedWritable', { name: target.name }), { tone: 'success' });
  };
  const actions = (
    <>
      {target?.writeAllowed === true && (
        <Button size="sm" variant="secondary" onClick={() => void clearWritable()}>
          {cs('clearWritable')}
        </Button>
      )}
      {status.plan && (
        <Button size="sm" variant="ghost" onClick={() => useCopyStore.getState().setStep('plan')}>
          {cs('reviewPlan')}
        </Button>
      )}
    </>
  );

  // The tables carry their own errors; a job-level reason is shown when none does.
  const jobProblem = (jobState === 'rejected' || jobState === 'failed') && !result.tables.some((t) => t.error);

  return (
    <div className="flex flex-col gap-4">
      {jobProblem && <JobEndNotice jobId={status.jobId} state={jobState} />}
      {result.dryRun && (
        <Notice tone="info" title={cs('dryRun')}>
          <div>{family === 'postgres' ? cs('dryRun.postgres') : cs('dryRun.other')}</div>
          <Button size="sm" variant="primary" className="mt-1.5" onClick={() => void useCopyStore.getState().editAgain({ replan: true })}>
            {cs('copyForReal')}
          </Button>
        </Notice>
      )}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 tabular-nums" data-testid="result-summary">
        <span className={done > 0 ? 'text-success' : 'text-fg-secondary'}>{cs('sum.done', { n: done })}</span>
        <span className={failed > 0 ? 'font-medium text-danger' : 'text-fg-secondary'}>{cs('sum.failed', { n: failed })}</span>
        <span className="text-fg-secondary">{cs('sum.skipped', { n: skipped })}</span>
        <span className="text-fg-secondary">{formatDuration(result.durationMs)}</span>
        {mismatches > 0 ? (
          <span className="font-medium text-danger">{cs('verifyMismatch', { n: mismatches })}</span>
        ) : result.verified ? (
          <span className="font-medium text-success">{cs('verified')}</span>
        ) : (
          !verifyRun && <span className="text-fg-tertiary">{cs('verifyNotRun')}</span>
        )}
      </div>
      {result.problems > 0 && <Notice tone="danger">{cs('problemsBanner', { n: result.problems })}</Notice>}
      {result.skippedExisting > 0 && <div className="text-fg-secondary">{cs('skippedExisting', { n: result.skippedExisting })}</div>}
      {status.plan?.resumeOf && <div className="text-sm text-fg-secondary">{cs('resumedPolicy', { policy: cs(k(`existing.${result.existing}`)) })}</div>}

      <VerificationTable result={result} items={items} />
      {checksum && <div className="text-sm text-fg-secondary">{cs('checksumCaveat')}</div>}

      {result.warnings.length > 0 && (
        <section className="flex flex-col gap-1">
          <Caption tone="warning">{cs('runtimeWarnings')}</Caption>
          <ul className="flex flex-col gap-0.5">
            {result.warnings.map((w, i) => (
              <li key={`${w.code}-${i}`} className="text-base" data-code={w.code}>
                {w.code === 'UNMAPPABLE_CHARS' ? cs('unmappableChars', { n: formatInt(result.tables.reduce((n, t) => n + Number(t.unmappableChars), 0)) }) : issueTitle(w)}
                <span className="ml-1.5 text-sm text-fg-secondary">{w.message}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      {result.leftBehind.length > 0 && (
        <Notice tone="warning" title={cs('leftBehind')}>
          <ul className="flex flex-col">
            {result.leftBehind.map((l, i) => (
              <li key={`${l.itemId}-${l.kind}-${i}`} data-kind={l.kind}>
                {label(l.itemId)}: {l.kind === 'partial_rows' ? cs('left.partial_rows', { rows: formatInt(l.rows ?? 0) }) : cs(k(`left.${l.kind}`))}
              </li>
            ))}
          </ul>
        </Notice>
      )}

      {resumable ? (
        <ResumePanel status={status} result={result}>
          {actions}
        </ResumePanel>
      ) : (
        <div className="flex flex-wrap items-center gap-2">{actions}</div>
      )}
    </div>
  );
}
