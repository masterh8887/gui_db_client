'use client';

// Jobs by id. The state of record lives in Rust: this store only applies `job:*` events and
// `job_list` / `job_get` snapshots; it never decides a state transition on its own.

import type {
  JobDone,
  JobProgress,
  JobServer,
  JobState,
  JobStateEvent,
  JobSummary,
  LogEvent,
} from 'ipc-types';
import { create } from 'zustand';
import { jobCancel, jobGet, jobList, jobResume } from '@/lib/commands';

export const ACTIVE_STATES: readonly JobState[] = ['pending', 'preflight', 'warned', 'running', 'finishing'];
export const TERMINAL_STATES: readonly JobState[] = ['done', 'cancelled', 'failed', 'rejected'];
export const ALL_STATES: readonly JobState[] = [...ACTIVE_STATES, ...TERMINAL_STATES];

export function isActiveState(s: JobState): boolean {
  return ACTIVE_STATES.includes(s);
}

export function isTerminalState(s: JobState): boolean {
  return TERMINAL_STATES.includes(s);
}

/** Per-job log lines kept in memory for the monitor card. */
export const JOB_LOG_LIMIT = 200;

export interface JobEntry {
  summary: JobSummary;
  progress?: JobProgress;
  server?: JobServer;
  logs: LogEvent[];
  /** `Date.now()` of the last event, for ordering "last results". */
  updatedAt: number;
}

export interface JobsState {
  jobs: Record<string, JobEntry>;
  /** Job ids in first-seen order. */
  order: string[];
  loaded: boolean;

  /** Store a `job_get` / `job_list` snapshot; a stale one never moves the state back (see `mergeSnapshot`). */
  upsertSummary(summary: JobSummary): void;
  upsertMany(list: JobSummary[]): void;
  /**
   * Apply `job:state`. Returns whether the job was already known and whether the caller must
   * fetch it with `refreshJob`: an unknown job needs its descriptive fields, and a job that
   * entered `warned` needs its preflight report, which only `job_get` / `job_list` carry (the
   * event has just `{jobId, state, at}`).
   */
  applyState(ev: JobStateEvent): { known: boolean; refetch: boolean };
  applyProgress(p: JobProgress): void;
  applyServer(s: JobServer): void;
  applyDone(d: JobDone): void;
  applyLog(l: LogEvent): void;
  remove(id: string): void;
  clearFinished(): void;

  load(): Promise<void>;
  refreshJob(id: string): Promise<JobSummary | undefined>;
  cancel(id: string): Promise<void>;
  resume(id: string, acknowledgedWarnings: string[]): Promise<void>;
}

function stubSummary(ev: JobStateEvent): JobSummary {
  return {
    id: ev.jobId,
    kind: 'export',
    state: ev.state,
    connId: '',
    connName: '',
    label: ev.jobId,
    startedAt: ev.at,
  };
}

/**
 * Position of a state in the job machine. Every core transition moves strictly forward
 * (pending -> preflight -> warned -> running -> finishing -> terminal; see `valid_transition`
 * in crates/core/src/job.rs), so a lower rank always means an older observation.
 */
const STATE_RANK: Record<JobState, number> = {
  pending: 0,
  preflight: 1,
  warned: 2,
  running: 3,
  finishing: 4,
  done: 5,
  cancelled: 5,
  failed: 5,
  rejected: 5,
};

/**
 * Combine a `job_get` / `job_list` snapshot with what the store already knows from events.
 *
 * Events and invoke responses travel on different channels, so a snapshot taken before the
 * latest `job:*` event may arrive after it. Such a stale snapshot must not move the job back
 * (in particular out of a terminal state, which is final): its descriptive fields (label,
 * connection, request) are taken, while the state and everything tied to it (preflight report,
 * result, error, exact count, finish time, counters) stay as the events left them. Returns
 * `snapshot` itself when it is at least as new as the stored state.
 */
export function mergeSnapshot(prev: JobSummary | undefined, snapshot: JobSummary): JobSummary {
  if (!prev) return snapshot;
  const older = STATE_RANK[snapshot.state] < STATE_RANK[prev.state];
  // Two different terminal states cannot both be true; the one the events delivered wins.
  const conflictingEnd = isTerminalState(prev.state) && isTerminalState(snapshot.state) && prev.state !== snapshot.state;
  if (!older && !conflictingEnd) return snapshot;
  return {
    ...snapshot,
    state: prev.state,
    preflight: prev.preflight,
    result: prev.result,
    error: prev.error,
    exactCount: prev.exactCount,
    finishedAt: prev.finishedAt,
    progress: prev.progress ?? snapshot.progress,
    server: prev.server ?? snapshot.server,
  };
}

export const useJobsStore = create<JobsState>()((set, get) => ({
  jobs: {},
  order: [],
  loaded: false,

  upsertSummary: (snapshot) =>
    set((s) => {
      const prev = s.jobs[snapshot.id];
      const summary = mergeSnapshot(prev?.summary, snapshot);
      // A stale snapshot's counters are older than the ones the events already delivered.
      const stale = summary !== snapshot;
      const entry: JobEntry = {
        summary,
        progress: stale ? (prev?.progress ?? snapshot.progress) : (snapshot.progress ?? prev?.progress),
        server: stale ? (prev?.server ?? snapshot.server) : (snapshot.server ?? prev?.server),
        logs: prev?.logs ?? [],
        updatedAt: Date.now(),
      };
      return {
        jobs: { ...s.jobs, [snapshot.id]: entry },
        order: prev ? s.order : [...s.order, snapshot.id],
      };
    }),

  upsertMany: (list) => {
    for (const summary of list) get().upsertSummary(summary);
    set({ loaded: true });
  },

  applyState: (ev) => {
    const known = !!get().jobs[ev.jobId];
    set((s) => {
      const prev = s.jobs[ev.jobId];
      const summary: JobSummary = prev ? { ...prev.summary, state: ev.state } : stubSummary(ev);
      if (isTerminalState(ev.state) && !summary.finishedAt) summary.finishedAt = ev.at;
      if (ev.state !== 'warned') summary.preflight = undefined;
      const entry: JobEntry = { ...(prev ?? { logs: [] }), summary, updatedAt: Date.now() };
      return {
        jobs: { ...s.jobs, [ev.jobId]: entry },
        order: prev ? s.order : [...s.order, ev.jobId],
      };
    });
    return { known, refetch: !known || ev.state === 'warned' };
  },

  applyProgress: (p) =>
    set((s) => {
      const prev = s.jobs[p.jobId];
      if (!prev) return s;
      return {
        jobs: {
          ...s.jobs,
          [p.jobId]: { ...prev, progress: p, summary: { ...prev.summary, progress: p }, updatedAt: Date.now() },
        },
      };
    }),

  applyServer: (sv) =>
    set((s) => {
      const prev = s.jobs[sv.jobId];
      if (!prev) return s;
      return {
        jobs: {
          ...s.jobs,
          [sv.jobId]: { ...prev, server: sv, summary: { ...prev.summary, server: sv }, updatedAt: Date.now() },
        },
      };
    }),

  applyDone: (d) =>
    set((s) => {
      const prev = s.jobs[d.jobId];
      const base: JobSummary = prev?.summary ?? {
        id: d.jobId,
        kind: d.exactCount ? 'exact_count' : 'export',
        state: d.state,
        connId: '',
        connName: '',
        label: d.jobId,
        startedAt: new Date().toISOString(),
      };
      const summary: JobSummary = {
        ...base,
        state: d.state,
        result: d.result ?? base.result,
        error: d.error ?? base.error,
        exactCount: d.exactCount ?? base.exactCount,
        preflight: undefined,
        finishedAt: base.finishedAt ?? new Date().toISOString(),
      };
      const entry: JobEntry = { ...(prev ?? { logs: [] }), summary, updatedAt: Date.now() };
      return {
        jobs: { ...s.jobs, [d.jobId]: entry },
        order: prev ? s.order : [...s.order, d.jobId],
      };
    }),

  applyLog: (l) => {
    if (!l.jobId) return;
    set((s) => {
      const prev = s.jobs[l.jobId!];
      if (!prev) return s;
      const logs = [...prev.logs, l];
      if (logs.length > JOB_LOG_LIMIT) logs.splice(0, logs.length - JOB_LOG_LIMIT);
      return { jobs: { ...s.jobs, [l.jobId!]: { ...prev, logs } } };
    });
  },

  remove: (id) =>
    set((s) => {
      if (!s.jobs[id]) return s;
      const jobs = { ...s.jobs };
      delete jobs[id];
      return { jobs, order: s.order.filter((x) => x !== id) };
    }),

  clearFinished: () =>
    set((s) => {
      const jobs: Record<string, JobEntry> = {};
      const order: string[] = [];
      for (const id of s.order) {
        const e = s.jobs[id];
        if (e && !isTerminalState(e.summary.state)) {
          jobs[id] = e;
          order.push(id);
        }
      }
      return { jobs, order };
    }),

  load: async () => {
    const list = await jobList();
    get().upsertMany(list);
  },

  refreshJob: async (id) => {
    try {
      get().upsertSummary(await jobGet(id));
      // The merged view: a stale snapshot does not override what later events said.
      return get().jobs[id]?.summary;
    } catch {
      return undefined;
    }
  },

  cancel: async (id) => {
    await jobCancel(id);
  },

  resume: async (id, acknowledgedWarnings) => {
    await jobResume(id, acknowledgedWarnings);
  },
}));

// ---- selectors (use with `useShallow` when the result is an array) -------------------------

export function selectAllJobs(s: JobsState): JobEntry[] {
  return s.order.map((id) => s.jobs[id]).filter((e): e is JobEntry => !!e);
}

export function selectActiveJobs(s: JobsState): JobEntry[] {
  return selectAllJobs(s).filter((e) => isActiveState(e.summary.state));
}

/** Terminal jobs, most recently finished first. */
export function selectRecentResults(s: JobsState, limit = 5): JobEntry[] {
  return selectAllJobs(s)
    .filter((e) => isTerminalState(e.summary.state))
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .slice(0, limit);
}

export function selectLastResult(s: JobsState): JobEntry | undefined {
  return selectRecentResults(s, 1)[0];
}

