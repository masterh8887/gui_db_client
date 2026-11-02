'use client';

// The DB copy wizard (docs/db-copy.md 7): `Sheet width="xl"`, mounted once in AppShell, open
// while `copy.wizard.open`. Steps in a SegmentedControl: 1 원본·대상, 2 개체, 3 옵션, 4 계획; then
// the 진행 and 결과 views of the job it shows. Steps 2-4 open only once both endpoints are valid.
// [복사 시작] / [모의 실행] stay disabled, with the reason as tooltip, until the plan has no
// blockers, every ack key is acknowledged, the target is write-allowed and a production target's
// name was typed. The sheet closes freely while a job runs (Activity's [자세히] reopens it).
// A job that ended without a result offers the way back: [다시 계획] (editing again, re-planned)
// for a new copy, [결과 보기] (the run's stored result, with its [이어서 실행]) for a resume.
// While mounted it listens to `copy:progress` (every copy job, for the Activity counts) and
// re-reads `copy_status` on every state change of the job it shows.

import type { CopyEndpoint, CopyServerFacts } from 'ipc-types';
import { useEffect, useId, useRef } from 'react';
import { Button, EnvTag, Notice, SegmentedControl, Sheet, type ButtonProps } from '@/components/ui';
import { askConfirm } from '@/lib/confirm';
import { connTag } from '@/lib/env';
import { EVENTS, listenTo } from '@/lib/events';
import { hasModKey } from '@/lib/platform';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore, type CopyStep } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { EndpointsStep } from './EndpointsStep';
import { continueBlock, driverLabel, endpointCaption, endpointProblem, factsEndpoint, isDefaultOptions, isDirtyDraft, isTerminal, pendingAckKeys, selectionEmpty, startBlock, startBlockText } from './model';
import { ObjectsStep } from './ObjectsStep';
import { OptionsStep } from './OptionsStep';
import { PlanStep } from './PlanStep';
import { ProgressView } from './ProgressView';
import { RecentRuns } from './RecentRuns';
import { ResultView } from './ResultView';
import { cs, k } from './strings';

const EDIT_STEPS: CopyStep[] = ['endpoints', 'objects', 'options', 'plan'];

/** `copy:progress` deltas → the store (merged by `seq`), while the wizard is mounted. */
function useCopyProgressEvents() {
  useEffect(() => {
    let off: (() => void) | undefined;
    let alive = true;
    void listenTo(EVENTS.copyProgress, (p) => useCopyStore.getState().applyProgress(p)).then((u) => {
      if (alive) off = u;
      else u();
    });
    return () => {
      alive = false;
      off?.();
    };
  }, []);
}

function EndpointText({ ep, facts }: { ep: CopyEndpoint; facts?: CopyServerFacts }) {
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === ep.connId));
  if (!conn) return <span className="text-fg-tertiary">—</span>;
  const caption = facts
    ? facts.driver === 'sqlite'
      ? cs('endpointSqlite', { path: facts.endpoint })
      : `${driverLabel(facts.driver)} · ${factsEndpoint(facts, ep.schema)}`
    : endpointCaption(conn, ep.database, ep.schema);
  const tag = connTag(conn);
  const text = `${conn.name} (${caption})`;
  return (
    <span className="inline-flex min-w-0 shrink items-center gap-1.5" title={text}>
      {tag && <EnvTag tag={tag} variant="chip" />}
      <span className="min-w-0 truncate">{text}</span>
    </span>
  );
}

/** A disabled button gets no pointer events: the reason hangs on an always-rendered wrapper. */
function GatedButton({ reason, children, ...props }: { reason: string | null } & ButtonProps) {
  const id = useId();
  return (
    <>
      <span className="inline-flex" title={reason ?? undefined}>
        <Button {...props} disabled={props.disabled || !!reason} aria-describedby={reason ? id : undefined}>
          {children}
        </Button>
      </span>
      {reason && (
        <span id={id} className="sr-only">
          {reason}
        </span>
      )}
    </>
  );
}

function WizardSheet() {
  const w = useCopyStore((s) => s.wizard);
  const status = useCopyStore((s) => (s.wizard.jobId ? s.runs[s.wizard.jobId] : undefined));
  const connections = useConnectionsStore((s) => s.connections);
  const jobState = useJobsStore((s) => (w.jobId ? s.jobs[w.jobId]?.summary.state : undefined));
  const store = useCopyStore.getState;
  const stepsRef = useRef<HTMLDivElement>(null);

  // Mod+Enter anywhere in the sheet: [복사 시작] on the plan step, [계속] for a warned job — only
  // when that button is enabled (the same gates; a dry run stays a click).
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key !== 'Enter' || !hasModKey(ev) || ev.shiftKey || ev.altKey || ev.isComposing) return;
      // Only from inside this sheet (the portal mounts after this effect; a confirm on top is another dialog).
      const dialog = stepsRef.current?.closest('[role="dialog"]');
      if (!dialog || !(ev.target instanceof Node) || !dialog.contains(ev.target)) return;
      const s = useCopyStore.getState();
      const wz = s.wizard;
      if (wz.starting) return;
      if (wz.jobId) {
        const st = s.runs[wz.jobId];
        const pending = pendingAckKeys(useJobsStore.getState().jobs[wz.jobId]?.summary.preflight?.warnings);
        if (st?.state !== 'warned' || continueBlock(st.plan ?? wz.plan, wz.acks, pending)) return;
        ev.preventDefault();
        void s.continueJob();
        return;
      }
      if (wz.step !== 'plan' || !wz.plan || startBlock(wz, useConnectionsStore.getState().connections)) return;
      ev.preventDefault();
      void s.start();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);

  // Re-read the job's status on every state change (repairs lost deltas, moves the steps along).
  useEffect(() => {
    if (w.jobId) void useCopyStore.getState().refreshStatus(w.jobId);
  }, [w.jobId, jobState]);

  const preflight = useJobsStore((s) => (w.jobId ? s.jobs[w.jobId]?.summary.preflight : undefined));
  const problem = endpointProblem(w.draft, connections);
  const jobMode = !!w.jobId;
  const state = jobState ?? status?.state;
  const warned = jobMode && status?.state === 'warned';
  const plan = w.plan ?? status?.plan ?? null;
  const idx = EDIT_STEPS.indexOf(w.step);
  // An empty "pick tables" selection makes a meaningless plan: block [다음] / [계획 만들기].
  const selEmpty = selectionEmpty(w.draft);

  const stepOptions = [
    ...EDIT_STEPS.map((s) => ({
      value: s,
      label: cs(k(`step.${s}`)),
      disabled: jobMode ? s !== 'plan' || !plan : s !== 'endpoints' && !!problem,
      title: !jobMode && s !== 'endpoints' && problem ? cs('stepLocked') : undefined,
    })),
    ...(jobMode
      ? [
          { value: 'progress' as const, label: cs('step.progress'), separatorBefore: true },
          { value: 'result' as const, label: cs('step.result'), disabled: !status?.result },
        ]
      : []),
  ];

  const close = () => store().close();
  // Esc / scrim / 취소 while editing a changed draft confirms first, so a stray dismissal does not
  // throw away the chosen tables, renames and overrides (spec 7; a job view closes freely).
  const requestClose = async () => {
    if (!jobMode && isDirtyDraft(w.draft)) {
      const ok = await askConfirm({ title: cs('discardTitle'), body: cs('discardBody'), confirmLabel: cs('discardConfirm'), danger: true });
      if (!ok) return;
    }
    close();
  };
  const gate = jobMode ? null : startBlock(w, connections);
  const gateText = gate ? startBlockText(gate) : null;
  // A warned job waits only on the keys still missing (its preflight), so acknowledgements already
  // given at start do not have to be re-ticked when the plan is reopened.
  const cont = warned ? continueBlock(plan, w.acks, pendingAckKeys(preflight?.warnings)) : null;
  // Ended without a result (rejected at start, failed before it had one): nothing to show but why.
  const endedEmpty = jobMode && !!status && isTerminal(state) && !status.result;
  // A resume runs as a new job of an earlier run (its run id is not its job id).
  const resumeJob = !!status && status.runId !== status.jobId;
  const direct = !problem && w.draft.selection.kind === 'all' && isDefaultOptions(w.draft.options);

  const cancelButton = (
    <Button variant="secondary" onClick={() => void requestClose()}>
      {jobMode ? cs('close') : cs('cancel')}
    </Button>
  );
  const back = idx > 0 && !jobMode && (
    <Button variant="secondary" onClick={() => store().setStep(EDIT_STEPS[idx - 1])}>
      {cs('back')}
    </Button>
  );

  let footer;
  if (jobMode) {
    footer = warned ? (
      <>
        {cancelButton}
        <GatedButton variant="primary" reason={cont ? startBlockText(cont) : null} loading={w.starting} onClick={() => void store().continueJob()}>
          {cs('continue')}
        </GatedButton>
      </>
    ) : endedEmpty && status ? (
      <>
        {cancelButton}
        {resumeJob ? (
          <Button variant="primary" onClick={() => void store().showRun(status.runId)}>
            {cs('runShow')}
          </Button>
        ) : (
          <Button variant="primary" onClick={() => void store().editAgain({ replan: true })}>
            {cs('replan')}
          </Button>
        )}
      </>
    ) : (
      cancelButton
    );
  } else if (w.step === 'plan') {
    footer = plan ? (
      <>
        {cancelButton}
        {back}
        <GatedButton variant="secondary" reason={gateText} onClick={() => void store().start({ dryRun: true })}>
          {cs('dryRun')}
        </GatedButton>
        <GatedButton variant="primary" reason={gateText} loading={w.starting} onClick={() => void store().start()}>
          {cs('startCopy')}
        </GatedButton>
      </>
    ) : (
      <>
        {cancelButton}
        {back}
        <GatedButton variant="primary" reason={problem ? cs('reason.endpoints') : w.planBusy ? cs('reason.planBusy') : selEmpty ? cs('noTablesSelected') : null} onClick={() => void store().buildPlan()}>
          {cs('makePlan')}
        </GatedButton>
      </>
    );
  } else {
    footer = (
      <>
        {cancelButton}
        {back}
        {w.step === 'endpoints' && direct && (
          <Button variant="secondary" onClick={() => void store().buildPlan()}>
            {cs('makePlan')}
          </Button>
        )}
        <GatedButton
          variant="primary"
          reason={problem ? cs('reason.endpoints') : selEmpty && (w.step === 'objects' || w.step === 'options') ? cs('noTablesSelected') : null}
          onClick={() => store().setStep(EDIT_STEPS[idx + 1])}
        >
          {cs('next')}
        </GatedButton>
      </>
    );
  }

  const description = (
    <span className="flex min-w-0 items-center gap-1.5">
      {w.draft.source.connId ? <EndpointText ep={w.draft.source} facts={plan?.source} /> : <span className="shrink-0 text-fg-tertiary">{cs('noSource')}</span>}
      <span className="shrink-0 text-fg-tertiary">→</span>
      {w.draft.target.connId ? <EndpointText ep={w.draft.target} facts={plan?.target} /> : <span className="text-fg-tertiary">{cs('noTarget')}</span>}
    </span>
  );

  return (
    <Sheet
      open
      width="xl"
      title={cs('title')}
      description={description}
      onOpenChange={(o) => {
        if (!o) void requestClose();
      }}
      className="h-[min(720px,calc(100vh-var(--sheet-top)-16px))]"
      footerStart={<RecentRuns disabled={jobMode && !isTerminal(state)} />}
      footer={footer}
    >
      {/* Sticks at the body's padding edge (-8px = its pt-2), so nothing scrolls by above it. */}
      <div ref={stepsRef} className="sticky -top-2 z-10 -mx-5 -mt-2 mb-3 border-b border-separator bg-elevated px-5 pt-2 pb-2">
        <SegmentedControl aria-label={cs('stepsAria')} value={w.step} onChange={(s) => store().setStep(s)} options={stepOptions} />
      </div>
      {w.error && w.step !== 'plan' && (
        <Notice tone="danger" className="mb-3">
          {w.error}
        </Notice>
      )}
      {w.step === 'endpoints' && <EndpointsStep />}
      {w.step === 'objects' && <ObjectsStep />}
      {w.step === 'options' && <OptionsStep />}
      {w.step === 'plan' && <PlanStep />}
      {w.step === 'progress' && <ProgressView status={status} />}
      {w.step === 'result' && <ResultView status={status} />}
    </Sheet>
  );
}

export function CopyWizard() {
  useCopyProgressEvents();
  const open = useCopyStore((s) => s.wizard.open);
  if (!open) return null;
  return <WizardSheet />;
}
