'use client';

// The DB copy wizard (docs/db-copy.md 7 "Stores"): one per window. Imports `connections`, `jobs`,
// `catalog`, `ui` only (plus lib helpers and the copy model).
//
// - `draft` never holds the start-time fields: `dryRun` stays false and `confirmProduction` is
//   absent; `start()` / `resume()` merge them (`confirmName` lives outside the draft, so typing
//   the production name never invalidates the plan).
// - Any draft change clears the plan. `buildPlan` keeps an answer only when the draft did not
//   change since the call (generation counter) and its plan token is still current.
// - `acks` are AckNeed keys; a new plan keeps only the keys present in it, so an old
//   acknowledgement never covers a different set of affected items.
// - Nothing starts without a plan free of blockers, a write-allowed target, every ack key and,
//   for a production target, its exact name (`startBlock`), also when called directly.
// - `runs` holds the `copy_status` of the jobs this window shows, merged by `seq`; `live` holds the
//   table counts of every copy job from `copy:progress` (Activity rows of any window).
// - A job that ends without a result (rejected by the start-time plan, or failed before it had
//   one) is never left on 진행 silently: a rejected job with a plan moves to 계획 (its blockers),
//   and the views show the job's error.
// - A resume carries only acknowledgements given for that run's own last plan.

import type {
  ColumnTreatment,
  Connection,
  ConnectionInput,
  CopyEndpoint,
  CopyOptions,
  CopyPlan,
  CopyProgress,
  CopyRequest,
  CopyRunDetail,
  CopyStatus,
  ExistingTablePolicy,
  JobState,
  ResumePartial,
} from 'ipc-types';
import { create } from 'zustand';
import defaults from '@/components/copy/copy-options-default.json';
import {
  cleanRenames,
  continueBlock,
  emptyProgress,
  isTerminal,
  mergeLive,
  mergeProgress,
  mergeStatus,
  pendingAckKeys,
  startBlock,
  startBlockText,
  type CopyLive,
} from '@/components/copy/model';
import { connGet, copyPlan, copyPlanCancel, copyResume, copyRun, copyStart, copyStatus } from '@/lib/commands';
import { errorText } from '@/lib/ipc-error';
import { useConnectionsStore } from './connections';
import { useJobsStore } from './jobs';

export type CopyStep = 'endpoints' | 'objects' | 'options' | 'plan' | 'progress' | 'result';

export interface CopyWizardState {
  open: boolean;
  step: CopyStep;
  /** dryRun false, no confirmProduction (start-time fields are merged in `start()` / `resume()`). */
  draft: CopyRequest;
  plan: CopyPlan | null;
  planBusy: boolean;
  planToken: string | null;
  error: string | null;
  /** AckNeed keys; a new plan keeps only keys present in it. */
  acks: string[];
  /** Start-time: the typed production target name. */
  confirmName: string;
  /** The copy job this wizard shows (progress, result, or a warned job's plan); null while editing. */
  jobId: string | null;
  /** A start / resume / continue request is in flight. */
  starting: boolean;
}

export interface CopyPreset {
  source?: Partial<CopyEndpoint>;
  target?: Partial<CopyEndpoint>;
}

export interface CopyState {
  wizard: CopyWizardState;
  /** By jobId: the jobs this window shows. */
  runs: Record<string, CopyStatus>;
  /** By jobId: table counts of every copy job seen on `copy:progress` (also jobs shown nowhere here). */
  live: Record<string, CopyLive>;
  open(preset?: CopyPreset): void;
  close(): void;
  /** Clears the plan. */
  setDraft(patch: Partial<CopyRequest>): void;
  setStep(s: CopyStep): void;
  toggleAck(key: string): void;
  setConfirmName(name: string): void;
  buildPlan(): Promise<void>;
  cancelPlan(): Promise<void>;
  exclude(itemId: string): Promise<void>;
  setItemPolicy(itemId: string, policy: ExistingTablePolicy | null): Promise<void>;
  start(opts?: { dryRun?: boolean }): Promise<string | null>;
  resume(runId: string, partial?: ResumePartial, confirmProduction?: string): Promise<string | null>;
  showJob(jobId: string): Promise<void>;
  applyProgress(p: CopyProgress): void;
  markWriteAllowed(connId: string): Promise<boolean>;
  clearWriteAllowed(connId: string): Promise<boolean>;

  // ---- extensions (copy-ui) ----
  /** MySQL: give one source table its own collation (`mysql.table_overrides`) and re-plan. */
  setTableCollation(table: string, collation: string): Promise<void>;
  /** MySQL: copy one source column as binary or text (`mysql.columns`) and re-plan. */
  setColumnTreatment(table: string, column: string, treatAs: ColumnTreatment): Promise<void>;
  /** Re-read `copy_status` of a job and merge it (also called on every `job:state` of the job). */
  refreshStatus(jobId: string): Promise<CopyStatus | null>;
  /** [계속] of a warned copy job: `job_resume(jobId, ackKeys)`. */
  continueJob(): Promise<boolean>;
  /** Show a stored run (최근 복사 → 결과 보기): its live job when this process has it, else the stored result. */
  showRun(runId: string): Promise<void>;
  /** Leave the job view and edit the request again (after a dry run: [실제로 복사] re-plans). */
  editAgain(opts?: { replan?: boolean }): Promise<void>;
}

/** Equals `components/copy/copy-options-default.json` (= the serde defaults of `CopyOptions`). */
export const DEFAULT_COPY_OPTIONS: CopyOptions = defaults as CopyOptions;

/** A fresh, deep copy of the default options (never shared between opens). */
export function defaultCopyOptions(): CopyOptions {
  return JSON.parse(JSON.stringify(DEFAULT_COPY_OPTIONS)) as CopyOptions;
}

function connectionOf(connId: string | undefined) {
  return connId ? useConnectionsStore.getState().connections.find((c) => c.id === connId) : undefined;
}

/**
 * One side of a fresh draft. The source database defaults to the connection's own database; the
 * target database starts empty unless preset (SQLite has only `main`).
 */
function endpoint(p: Partial<CopyEndpoint> | undefined, side: 'source' | 'target'): CopyEndpoint {
  const connId = p?.connId ?? '';
  const conn = connectionOf(connId);
  const fallback = conn?.driver === 'sqlite' ? 'main' : side === 'source' ? (conn?.database ?? '') : '';
  return { connId, database: p?.database ?? fallback, ...(p?.schema ? { schema: p.schema } : {}) };
}

function freshWizard(preset?: CopyPreset, open = false): CopyWizardState {
  return {
    open,
    step: 'endpoints',
    draft: { source: endpoint(preset?.source, 'source'), target: endpoint(preset?.target, 'target'), selection: { kind: 'all' }, options: defaultCopyOptions() },
    plan: null,
    planBusy: false,
    planToken: null,
    error: null,
    acks: [],
    confirmName: '',
    jobId: null,
    starting: false,
  };
}

/** The request as stored for a run: no passwords, no start-time fields. */
function cleanRequest(req: CopyRequest): CopyRequest {
  const { confirmProduction: _c, ...options } = req.options;
  void _c;
  const side = (e: CopyEndpoint): CopyEndpoint => ({ connId: e.connId, database: e.database, ...(e.schema ? { schema: e.schema } : {}) });
  return { ...req, source: side(req.source), target: side(req.target), options: { ...defaultCopyOptions(), ...options, dryRun: false } };
}

/** The stored connection (`conn_get`), else the listed one. */
async function freshConnection(connId: string): Promise<Connection | undefined> {
  if (!connId) return undefined;
  try {
    return await connGet(connId);
  } catch {
    return connectionOf(connId);
  }
}

/** A job rejected by its start-time plan (the blockers are in that plan), without a result. */
function rejectedWithPlan(s: CopyStatus): s is CopyStatus & { plan: CopyPlan } {
  return s.state === 'rejected' && !!s.plan && !s.result;
}

let tokenSeq = 0;
/** Bumped by every draft change (and by `open`): `buildPlan` keeps only answers of its own generation. */
let draftGen = 0;

export const useCopyStore = create<CopyState>()((set, get) => {
  const patchWizard = (patch: Partial<CopyWizardState>) => set((s) => ({ wizard: { ...s.wizard, ...patch } }));
  const connections = () => useConnectionsStore.getState().connections;

  const setWriteAllowed = async (connId: string, writeAllowed: boolean): Promise<boolean> => {
    try {
      const stored = await connGet(connId);
      const { hasPassword: _hp, hasSshSecret: _hs, ...rest } = stored;
      void _hp;
      void _hs;
      await useConnectionsStore.getState().save({ ...rest, hasPassword: false, hasSshSecret: false, writeAllowed } as ConnectionInput);
      await useConnectionsStore.getState().load();
      return true;
    } catch (e) {
      patchWizard({ error: errorText(e) });
      return false;
    }
  };

  /** Show `jobId` in the wizard: a placeholder status until `copy_status` answers. */
  const enterJob = (jobId: string, plan: CopyPlan | null) => {
    set((s) => ({
      runs: s.runs[jobId]
        ? s.runs
        : { ...s.runs, [jobId]: { jobId, runId: jobId, state: 'pending', ...(plan ? { plan } : {}), progress: emptyProgress(jobId, jobId) } },
      wizard: { ...s.wizard, open: true, jobId, step: 'progress', error: null, starting: false, confirmName: '' },
    }));
  };

  /**
   * Move the wizard along with its job: warned → its plan (acknowledgements); rejected by the
   * start-time plan → that plan (its blockers); a warned job that continued (here, in Activity or
   * in another window) → progress; finished with a result → result.
   */
  const follow = (prevState: JobState | undefined, status: CopyStatus) => {
    const w = get().wizard;
    if (w.jobId !== status.jobId) return;
    if (status.state === 'warned' && status.plan && w.step === 'progress') {
      const keys = new Set(status.plan.acks.map((a) => a.key));
      patchWizard({ step: 'plan', plan: status.plan, acks: w.acks.filter((x) => keys.has(x)) });
    } else if (rejectedWithPlan(status) && prevState !== 'rejected' && w.step === 'progress') {
      // Once, when the rejection is first seen: [진행] stays reachable afterwards.
      patchWizard({ step: 'plan', plan: status.plan, acks: [] });
    } else if (prevState === 'warned' && status.state !== 'warned' && w.step === 'plan') {
      patchWizard({ step: isTerminal(status.state) && status.result ? 'result' : 'progress' });
    } else if (status.result && isTerminal(status.state) && w.step === 'progress') {
      patchWizard({ step: 'result', ...(status.plan ? { plan: status.plan } : {}) });
    } else if (status.plan && !w.plan) {
      patchWizard({ plan: status.plan });
    }
  };

  const loadDraftOf = async (runId: string): Promise<CopyRunDetail | null> => {
    try {
      const detail = await copyRun(runId);
      patchWizard({ draft: cleanRequest(detail.request) });
      return detail;
    } catch {
      return null;
    }
  };

  return {
    wizard: freshWizard(),
    runs: {},
    live: {},

    open: (preset) => {
      const token = get().wizard.planToken;
      if (token) void copyPlanCancel(token).catch(() => undefined);
      draftGen += 1;
      set({ wizard: freshWizard(preset, true) });
    },
    close: () => {
      const token = get().wizard.planToken;
      if (token) void copyPlanCancel(token).catch(() => undefined);
      patchWizard({ open: false, planBusy: false, planToken: null });
    },
    setDraft: (patch) => {
      draftGen += 1;
      set((s) => ({ wizard: { ...s.wizard, draft: { ...s.wizard.draft, ...patch }, plan: null, error: null } }));
    },
    setStep: (step) => patchWizard({ step }),
    toggleAck: (key) =>
      set((s) => {
        const acks = s.wizard.acks.includes(key) ? s.wizard.acks.filter((x) => x !== key) : [...s.wizard.acks, key];
        return { wizard: { ...s.wizard, acks } };
      }),
    setConfirmName: (confirmName) => patchWizard({ confirmName }),

    buildPlan: async () => {
      const old = get().wizard.planToken;
      if (old) void copyPlanCancel(old).catch(() => undefined);
      // Drop incomplete rename rows (empty from/to) so the plan — and the start request built from
      // this draft — never names an empty target.
      const cleaned = cleanRenames(get().wizard.draft.options);
      if (cleaned !== get().wizard.draft.options) set((s) => ({ wizard: { ...s.wizard, draft: { ...s.wizard.draft, options: cleaned } } }));
      const planToken = `plan-${Date.now()}-${++tokenSeq}`;
      const gen = draftGen;
      patchWizard({ planBusy: true, planToken, error: null, plan: null, step: 'plan' });
      const current = () => get().wizard.planToken === planToken;
      try {
        const plan = await copyPlan(get().wizard.draft, planToken);
        if (!current() || gen !== draftGen) return;
        const keys = new Set(plan.acks.map((a) => a.key));
        patchWizard({ plan, acks: get().wizard.acks.filter((x) => keys.has(x)) });
      } catch (e) {
        if (current() && gen === draftGen) patchWizard({ error: errorText(e) });
      } finally {
        if (current()) patchWizard({ planBusy: false, planToken: null });
      }
    },
    cancelPlan: async () => {
      const token = get().wizard.planToken;
      patchWizard({ planBusy: false, planToken: null });
      if (token) await copyPlanCancel(token).catch(() => undefined);
    },
    exclude: async (itemId) => {
      const { options } = get().wizard.draft;
      if (options.exclude.includes(itemId)) return;
      get().setDraft({ options: { ...options, exclude: [...options.exclude, itemId] } });
      await get().buildPlan();
    },
    setItemPolicy: async (itemId, policy) => {
      const { options } = get().wizard.draft;
      const rest = options.existingOverrides.filter((o) => o.itemId !== itemId);
      const existingOverrides = policy ? [...rest, { itemId, policy }] : rest;
      get().setDraft({ options: { ...options, existingOverrides } });
      await get().buildPlan();
    },
    setTableCollation: async (table, collation) => {
      const { options } = get().wizard.draft;
      const rest = options.mysql.tableOverrides.filter((o) => o.table !== table);
      const prev = options.mysql.tableOverrides.find((o) => o.table === table);
      const tableOverrides = [...rest, { ...prev, table, collation }];
      get().setDraft({ options: { ...options, mysql: { ...options.mysql, tableOverrides } } });
      await get().buildPlan();
    },
    setColumnTreatment: async (table, column, treatAs) => {
      const { options } = get().wizard.draft;
      const rest = options.mysql.columns.filter((c) => !(c.table === table && c.column === column));
      const columns = [...rest, { table, column, treatAs }];
      get().setDraft({ options: { ...options, mysql: { ...options.mysql, columns } } });
      await get().buildPlan();
    },

    start: async (opts) => {
      if (get().wizard.jobId || get().wizard.starting) return null;
      // The gate reads the stored target (core is the state of record), so a write access
      // cleared in another window or process counts here before `connections:changed` arrives.
      const target = await freshConnection(get().wizard.draft.target.connId);
      const { wizard } = get();
      const conns = target ? [...connections().filter((c) => c.id !== target.id), target] : connections();
      const blocked = startBlock(wizard, conns);
      if (blocked || !wizard.plan) {
        if (blocked) patchWizard({ error: startBlockText(blocked) });
        return null;
      }
      const options: CopyOptions = { ...wizard.draft.options, dryRun: opts?.dryRun ?? false };
      delete options.confirmProduction;
      if (target?.tag === 'production') options.confirmProduction = wizard.confirmName;
      const req: CopyRequest = { ...wizard.draft, options };
      const plan = wizard.plan;
      patchWizard({ starting: true, error: null });
      try {
        const jobId = await copyStart(req, plan.planId, wizard.acks);
        useJobsStore.getState().markLocal(jobId);
        enterJob(jobId, plan);
        await get().refreshStatus(jobId);
        return jobId;
      } catch (e) {
        patchWizard({ starting: false, error: errorText(e) });
        return null;
      }
    },
    resume: async (runId, partial, confirmProduction) => {
      // Only keys acknowledged for this run's own last plan carry over, never those of another
      // plan the wizard was editing (core plans again; anything missing makes the job wait in
      // `warned`, where the wizard asks).
      const { wizard, runs } = get();
      const own = wizard.jobId ? runs[wizard.jobId] : undefined;
      const ownKeys = new Set(own?.runId === runId ? (own.plan?.acks.map((a) => a.key) ?? []) : []);
      const acknowledged = wizard.acks.filter((x) => ownKeys.has(x));
      patchWizard({ starting: true, error: null });
      try {
        const jobId = await copyResume({
          runId,
          acknowledged,
          partial: partial ?? 'fail',
          ...(confirmProduction ? { confirmProduction } : {}),
        });
        useJobsStore.getState().markLocal(jobId);
        enterJob(jobId, null);
        set((s) => ({ wizard: { ...s.wizard, plan: null, acks: [] } }));
        await get().refreshStatus(jobId);
        return jobId;
      } catch (e) {
        patchWizard({ starting: false, error: errorText(e) });
        return null;
      }
    },
    continueJob: async () => {
      const { wizard, runs } = get();
      const jobId = wizard.jobId;
      const status = jobId ? runs[jobId] : undefined;
      if (!jobId || status?.state !== 'warned') return false;
      // Only the keys core is still waiting for (its preflight) must be ticked; ones given at start
      // need not be re-entered when the warned plan is reopened from another window.
      const pending = pendingAckKeys(useJobsStore.getState().jobs[jobId]?.summary.preflight?.warnings);
      const blocked = continueBlock(status.plan ?? wizard.plan, wizard.acks, pending);
      if (blocked) {
        patchWizard({ error: startBlockText(blocked) });
        return false;
      }
      patchWizard({ starting: true, error: null });
      try {
        await useJobsStore.getState().resume(jobId, wizard.acks);
        patchWizard({ starting: false, step: 'progress' });
        await get().refreshStatus(jobId);
        return true;
      } catch (e) {
        patchWizard({ starting: false, error: errorText(e) });
        return false;
      }
    },

    refreshStatus: async (jobId) => {
      try {
        const next = await copyStatus(jobId);
        const prevState = get().runs[jobId]?.state;
        set((s) => ({ runs: { ...s.runs, [jobId]: mergeStatus(s.runs[jobId], next) } }));
        const merged = get().runs[jobId];
        follow(prevState, merged);
        return merged;
      } catch {
        return null;
      }
    },
    showJob: async (jobId) => {
      try {
        const status = await copyStatus(jobId);
        set((s) => ({ runs: { ...s.runs, [jobId]: mergeStatus(s.runs[jobId], status) } }));
        const merged = get().runs[jobId];
        const step: CopyStep = merged.state === 'warned' || rejectedWithPlan(merged) ? 'plan' : merged.result ? 'result' : 'progress';
        const keep = get().wizard.jobId === jobId ? get().wizard.acks : [];
        const keys = new Set(merged.plan?.acks.map((a) => a.key) ?? []);
        set((s) => ({
          wizard: {
            ...s.wizard,
            open: true,
            jobId,
            step,
            plan: merged.plan ?? null,
            planBusy: false,
            planToken: null,
            starting: false,
            error: null,
            acks: keep.filter((x) => keys.has(x)),
          },
        }));
        await loadDraftOf(merged.runId);
      } catch (e) {
        patchWizard({ open: true, error: errorText(e) });
      }
    },
    showRun: async (runId) => {
      let detail: CopyRunDetail;
      try {
        detail = await copyRun(runId);
      } catch (e) {
        patchWizard({ error: errorText(e) });
        return;
      }
      const jobId = detail.info.lastJobId;
      const live = await copyStatus(jobId).catch(() => null);
      // A live job that ended without a result (a resume rejected or failed at start) has nothing
      // to resume from: the run's stored result, with its [이어서 실행], is what is shown then.
      if (live && !(isTerminal(live.state) && !live.result && detail.result)) {
        await get().showJob(jobId);
        return;
      }
      // Another process ran it, this one restarted, or the last job ended without a result.
      const status: CopyStatus = { jobId, runId, state: live?.state ?? detail.info.state, progress: emptyProgress(jobId, runId), ...(detail.result ? { result: detail.result } : {}) };
      set((s) => ({
        runs: { ...s.runs, [jobId]: status },
        wizard: {
          ...s.wizard,
          open: true,
          jobId,
          step: detail.result ? 'result' : 'progress',
          plan: null,
          planBusy: false,
          planToken: null,
          starting: false,
          error: null,
          acks: [],
          confirmName: '',
          draft: cleanRequest(detail.request),
        },
      }));
    },
    editAgain: async (opts) => {
      draftGen += 1;
      patchWizard({ jobId: null, step: 'plan', plan: null, error: null, confirmName: '', starting: false });
      if (opts?.replan) await get().buildPlan();
    },

    applyProgress: (p) =>
      set((s) => {
        // Counts of every copy job (Activity rows), whichever window started it.
        const counts = mergeLive(s.live[p.jobId], p);
        const live = counts === s.live[p.jobId] ? s.live : { ...s.live, [p.jobId]: counts };
        const prev = s.runs[p.jobId];
        if (!prev) {
          // Only the job this wizard shows gets a status; the next `copy_status` fills the rest.
          if (s.wizard.jobId !== p.jobId) return live === s.live ? s : { live };
          const state = useJobsStore.getState().jobs[p.jobId]?.summary.state ?? 'running';
          return { live, runs: { ...s.runs, [p.jobId]: { jobId: p.jobId, runId: p.runId, state, progress: { ...p, full: true } } } };
        }
        return { live, runs: { ...s.runs, [p.jobId]: { ...prev, progress: mergeProgress(prev.progress, p) } } };
      }),
    markWriteAllowed: (connId) => setWriteAllowed(connId, true),
    clearWriteAllowed: (connId) => setWriteAllowed(connId, false),
  };
});
