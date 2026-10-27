import type { JobDone, JobProgress, JobServer, JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { resetStores } from '@/test/stores';
import { ALL_STATES, mergeSnapshot, selectActiveJobs, selectLastResult, selectRecentResults, useJobsStore } from './jobs';

const at = '2026-10-26T10:00:00.000Z';

function summary(id: string, state: JobState): JobSummary {
  return { id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: `job ${id}`, startedAt: at };
}

describe('jobs store', () => {
  beforeEach(() => resetStores());

  it('creates a stub for an unknown job on job:state and reports it as unknown', () => {
    const { known } = useJobsStore.getState().applyState({ jobId: 'j1', state: 'pending', at });
    expect(known).toBe(false);
    const entry = useJobsStore.getState().jobs.j1;
    expect(entry.summary.state).toBe('pending');
    expect(entry.summary.id).toBe('j1');
    expect(useJobsStore.getState().order).toEqual(['j1']);
    expect(useJobsStore.getState().applyState({ jobId: 'j1', state: 'preflight', at }).known).toBe(true);
  });

  it('walks the happy path pending -> preflight -> running -> finishing -> done', () => {
    const s = useJobsStore.getState();
    s.upsertSummary(summary('j1', 'pending'));
    for (const st of ['preflight', 'running', 'finishing'] as JobState[]) {
      s.applyState({ jobId: 'j1', state: st, at });
      expect(useJobsStore.getState().jobs.j1.summary.state).toBe(st);
      expect(selectActiveJobs(useJobsStore.getState()).map((e) => e.summary.id)).toEqual(['j1']);
    }
    const progress: JobProgress = {
      jobId: 'j1',
      rows: BigInt(1000),
      bytesIn: BigInt(2000),
      bytesOut: BigInt(1900),
      batches: BigInt(1),
      files: 1,
      elapsedMs: BigInt(500),
      rateRowsPerSec: 2000,
      rateBytesPerSec: 3800,
      etaSec: BigInt(10),
      phase: 'transferring',
    };
    s.applyProgress(progress);
    expect(useJobsStore.getState().jobs.j1.progress).toEqual(progress);
    expect(useJobsStore.getState().jobs.j1.summary.progress).toEqual(progress);

    const server: JobServer = { jobId: 'j1', pid: '1', state: 'active', waitEvent: 'ClientWrite', bottleneck: 'network' };
    s.applyServer(server);
    expect(useJobsStore.getState().jobs.j1.server).toEqual(server);

    const done: JobDone = {
      jobId: 'j1',
      state: 'done',
      result: { rows: BigInt(1000), bytes: BigInt(1900), files: ['C:\\x.csv'], durationMs: BigInt(900), notes: [] },
    };
    s.applyDone(done);
    const final = useJobsStore.getState().jobs.j1.summary;
    expect(final.state).toBe('done');
    expect(final.result?.files).toEqual(['C:\\x.csv']);
    expect(final.finishedAt).toBeTruthy();
    expect(selectActiveJobs(useJobsStore.getState())).toHaveLength(0);
    expect(selectLastResult(useJobsStore.getState())?.summary.id).toBe('j1');
  });

  it('handles warned -> (ack) -> running and keeps the preflight only while warned', () => {
    const s = useJobsStore.getState();
    s.upsertSummary({
      ...summary('j2', 'warned'),
      preflight: { warnings: [], blockers: [], estimates: { outputPath: 'x' }, sessions: [], elapsedMs: BigInt(1) },
    });
    expect(useJobsStore.getState().jobs.j2.summary.preflight).toBeDefined();
    s.applyState({ jobId: 'j2', state: 'running', at });
    expect(useJobsStore.getState().jobs.j2.summary.state).toBe('running');
    expect(useJobsStore.getState().jobs.j2.summary.preflight).toBeUndefined();
  });

  it.each(['rejected', 'cancelled', 'failed'] as JobState[])('terminal state %s via job:done carries the error', (st) => {
    const s = useJobsStore.getState();
    s.upsertSummary(summary('j3', st === 'rejected' ? 'preflight' : 'running'));
    s.applyDone({ jobId: 'j3', state: st, error: { code: st === 'cancelled' ? 'CANCELLED' : 'QUERY', message: 'boom' } });
    const sum = useJobsStore.getState().jobs.j3.summary;
    expect(sum.state).toBe(st);
    expect(sum.error?.message).toBe('boom');
    expect(selectActiveJobs(useJobsStore.getState())).toHaveLength(0);
    expect(selectRecentResults(useJobsStore.getState()).map((e) => e.summary.id)).toEqual(['j3']);
  });

  it('accepts every one of the nine states through job:state', () => {
    const s = useJobsStore.getState();
    for (const st of ALL_STATES) {
      s.applyState({ jobId: `j-${st}`, state: st, at });
      expect(useJobsStore.getState().jobs[`j-${st}`].summary.state).toBe(st);
    }
    expect(ALL_STATES).toHaveLength(9);
    expect(selectActiveJobs(useJobsStore.getState())).toHaveLength(5);
    expect(selectRecentResults(useJobsStore.getState(), 10)).toHaveLength(4);
  });

  it('attaches job logs only to known jobs and caps them', () => {
    const s = useJobsStore.getState();
    s.applyLog({ jobId: 'nope', level: 'info', message: 'x', at });
    expect(useJobsStore.getState().jobs.nope).toBeUndefined();
    s.upsertSummary(summary('j4', 'running'));
    for (let i = 0; i < 250; i += 1) s.applyLog({ jobId: 'j4', level: 'info', message: `line ${i}`, at });
    const logs = useJobsStore.getState().jobs.j4.logs;
    expect(logs).toHaveLength(200);
    expect(logs[0].message).toBe('line 50');
  });

  it('exact counts arrive through job:done and are kept on the summary', () => {
    const s = useJobsStore.getState();
    s.upsertSummary({ ...summary('c1', 'running'), kind: 'exact_count' });
    s.applyDone({ jobId: 'c1', state: 'done', exactCount: { table: { schema: 'public', name: 'orders' }, rows: BigInt(5), elapsedMs: BigInt(3), at } });
    expect(useJobsStore.getState().jobs.c1.summary.exactCount?.rows).toBe(BigInt(5));
  });

  it('asks for a refetch for unknown jobs and whenever a job enters warned', () => {
    const s = useJobsStore.getState();
    expect(s.applyState({ jobId: 'w1', state: 'pending', at })).toEqual({ known: false, refetch: true });
    expect(s.applyState({ jobId: 'w1', state: 'preflight', at })).toEqual({ known: true, refetch: false });
    // Known by now, but the report the user must acknowledge only comes with job_get.
    expect(s.applyState({ jobId: 'w1', state: 'warned', at })).toEqual({ known: true, refetch: true });
    expect(useJobsStore.getState().jobs.w1.summary.preflight).toBeUndefined();
    expect(s.applyState({ jobId: 'w1', state: 'running', at })).toEqual({ known: true, refetch: false });
  });

  it('never lets a stale snapshot move a terminal job back to an active state', () => {
    const s = useJobsStore.getState();
    s.applyState({ jobId: 'j5', state: 'pending', at });
    const result = { rows: BigInt(3), bytes: BigInt(30), files: ['C:\\small.csv'], durationMs: BigInt(40), notes: [] };
    s.applyDone({ jobId: 'j5', state: 'done', result });
    // The job_get answer started by the `pending` event, taken while the job was still running.
    s.upsertSummary({ ...summary('j5', 'running'), label: 'orders small' });
    const sum = useJobsStore.getState().jobs.j5.summary;
    expect(sum.state).toBe('done');
    expect(sum.result).toEqual(result);
    expect(sum.finishedAt).toBeTruthy();
    // Descriptive fields still come from the snapshot.
    expect(sum.label).toBe('orders small');
    expect(sum.connId).toBe('c-prod');
    expect(selectActiveJobs(useJobsStore.getState())).toHaveLength(0);

    // Same for errors and a different (impossible) terminal state.
    s.applyDone({ jobId: 'j6', state: 'rejected', error: { code: 'INVALID', message: 'not a SELECT' } });
    s.upsertSummary(summary('j6', 'preflight'));
    s.upsertSummary(summary('j6', 'failed'));
    expect(useJobsStore.getState().jobs.j6.summary.state).toBe('rejected');
    expect(useJobsStore.getState().jobs.j6.summary.error?.message).toBe('not a SELECT');
  });

  it('keeps the newer active state over an older snapshot and takes the preflight report from a current one', () => {
    const s = useJobsStore.getState();
    s.upsertSummary(summary('j7', 'pending'));
    s.applyState({ jobId: 'j7', state: 'preflight', at });
    s.applyState({ jobId: 'j7', state: 'warned', at });
    s.upsertSummary(summary('j7', 'preflight'));
    expect(useJobsStore.getState().jobs.j7.summary.state).toBe('warned');
    const preflight = { warnings: [{ code: 'OVERWRITE', severity: 'warning' as const, message: 'exists' }], blockers: [], estimates: { outputPath: 'x' }, sessions: [], elapsedMs: BigInt(1) };
    s.upsertSummary({ ...summary('j7', 'warned'), preflight });
    expect(useJobsStore.getState().jobs.j7.summary.preflight).toEqual(preflight);
    s.applyState({ jobId: 'j7', state: 'running', at });
    // A late warned snapshot neither brings the job back nor re-attaches the report.
    s.upsertSummary({ ...summary('j7', 'warned'), preflight });
    expect(useJobsStore.getState().jobs.j7.summary.state).toBe('running');
    expect(useJobsStore.getState().jobs.j7.summary.preflight).toBeUndefined();
  });

  it('mergeSnapshot returns the snapshot itself when it is current', () => {
    const snap = summary('m1', 'running');
    expect(mergeSnapshot(undefined, snap)).toBe(snap);
    expect(mergeSnapshot(summary('m1', 'preflight'), snap)).toBe(snap);
    expect(mergeSnapshot(summary('m1', 'running'), snap)).toBe(snap);
    expect(mergeSnapshot(summary('m1', 'finishing'), snap)).not.toBe(snap);
  });

  it('clearFinished keeps active jobs only', () => {
    const s = useJobsStore.getState();
    s.upsertSummary(summary('a', 'running'));
    s.upsertSummary(summary('b', 'done'));
    s.clearFinished();
    expect(useJobsStore.getState().order).toEqual(['a']);
  });
});
