import type { JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { askCancel, askCancelLatest, reviewWarnings, showActivity } from './job-actions';

const job = (id: string, state: JobState): JobSummary => ({ id, kind: 'export', state, connId: 'c-prod', connName: 'prod', label: id, startedAt: '2026-10-27T00:00:00Z' });

describe('job actions', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });

  it('showActivity opens the activity popover on Welcome and the inspector on 활동 in a workspace', async () => {
    showActivity();
    expect(useUiStore.getState().activityPopoverOpen).toBe(true);
    expect(useUiStore.getState().inspectorOpen).toBe(false);
    await useWorkspaceStore.getState().openWorkspace('c-local');
    useUiStore.getState().setInspectorSegment('details');
    showActivity();
    expect(useUiStore.getState()).toMatchObject({ inspectorOpen: true, inspectorSegment: 'activity' });
  });

  it('Esc / Mod+.: asks for the newest active job, a second press dismisses; nothing active → nothing', () => {
    askCancelLatest();
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    useJobsStore.getState().upsertSummary(job('a', 'running'));
    useJobsStore.getState().upsertSummary(job('b', 'running'));
    useJobsStore.getState().upsertSummary(job('c', 'done'));
    askCancelLatest();
    expect(useUiStore.getState().cancelConfirmJobId).toBe('b');
    askCancelLatest();
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    askCancel('a');
    expect(useUiStore.getState().cancelConfirmJobId).toBe('a');
    // The job state itself is untouched (Rust decides).
    expect(useJobsStore.getState().jobs.a.summary.state).toBe('running');
  });

  it('reviewWarnings opens the resume sheet, fetching the report when missing', async () => {
    expect(await reviewWarnings('nope')).toBe(false);
    const report = { warnings: [{ code: 'OVERWRITE', severity: 'warning' as const, message: 'x' }], blockers: [], estimates: { outputPath: 'x' }, sessions: [], elapsedMs: BigInt(1) };
    useJobsStore.getState().upsertSummary({ ...job('w', 'warned'), preflight: report });
    expect(await reviewWarnings('w')).toBe(true);
    expect(useExtractStore.getState().preflightDialog).toMatchObject({ kind: 'resume', jobId: 'w', busy: false, error: null });
  });
});
