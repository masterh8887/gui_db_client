// A warned copy job (docs/db-copy.md 5.1 step 3, 7 "Activity") through the browser mock, as core
// makes one: a start whose plan changed since the review, or whose acknowledgements are missing,
// returns a job that waits in `warned`. Activity's [검토] / `reviewWarnings` open the wizard on the
// plan step with `copy_status.plan`, its acknowledgement checkboxes and the PLAN_CHANGED
// highlights; [계속] calls `job_resume(jobId, ackKeys)`; once the job runs, the wizard follows it.

import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { CopyPlan, CopyRequest } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { TooltipProvider } from '@/components/ui';
import { connSave, copyPlan, copyStart, copyStatus } from '@/lib/commands';
import { subscribeAll } from '@/lib/events';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { DEFAULT_COPY_OPTIONS, useCopyStore } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { resetStores } from '@/test/stores';
import { cs } from './strings';

// The real commands over the mock backend, with `jobResume` observable.
vi.mock('@/lib/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/commands')>();
  return { ...actual, jobResume: vi.fn(actual.jobResume) };
});

const { reviewWarnings } = await import('@/lib/job-actions');
const { jobResume } = await import('@/lib/commands');
const { CopyWizard } = await import('./CopyWizard');

const wiz = () => useCopyStore.getState().wizard;
const keysOf = (p: CopyPlan) => p.acks.map((a) => a.key);

function request(): CopyRequest {
  return {
    source: { connId: 'c-prod', database: 'orders', schema: 'public' },
    target: { connId: 'c-staging', database: 'analytics_dw' },
    selection: { kind: 'all' },
    options: structuredClone(DEFAULT_COPY_OPTIONS),
  };
}

function renderWizard() {
  return render(
    <TooltipProvider>
      <CopyWizard />
    </TooltipProvider>,
  );
}

async function waitWarned(jobId: string): Promise<void> {
  await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.state).toBe('warned'), { timeout: 3000 });
}

describe('warned copy job', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, progressTickMs: 4 });
    const staging = mockConnections().find((c) => c.id === 'c-staging')!;
    await connSave({ ...staging, writeAllowed: true });
    useConnectionsStore.setState({ connections: mockConnections().map((c) => (c.id === 'c-staging' ? { ...c, writeAllowed: true } : c)), drivers: mockDrivers(), loaded: true });
    // What AppEvents does for the jobs store (a warned job's report comes with `job_get`).
    await subscribeAll({
      onJobState: (ev) => {
        const r = useJobsStore.getState().applyState(ev);
        if (!r.known || r.refetch) void useJobsStore.getState().refreshJob(ev.jobId);
      },
      onJobProgress: (p) => useJobsStore.getState().applyProgress(p),
      onJobDone: (d) => useJobsStore.getState().applyDone(d),
    });
    vi.mocked(jobResume).mockClear();
  });

  it('a changed plan: [검토] opens the plan step with the highlights; [계속] resumes with the ack keys and follows the job', async () => {
    const user = userEvent.setup();
    renderWizard();
    const reviewed = await copyPlan(request());
    // Started with a request that no longer matches the reviewed plan: one table is left out.
    const changed = request();
    changed.options.exclude = ['table:"public"."orders"'];
    const jobId = await copyStart(changed, reviewed.planId, keysOf(reviewed));
    await waitWarned(jobId);
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.preflight?.warnings.length).toBeGreaterThan(0));

    expect(await reviewWarnings(jobId)).toBe(true);
    const dialog = await screen.findByRole('dialog');
    expect(wiz()).toMatchObject({ open: true, jobId, step: 'plan' });
    expect(within(dialog).getByText(cs('warnedJob'))).toBeInTheDocument();
    expect(within(dialog).getByText(cs('planChangedNotice'))).toBeInTheDocument();
    const row = (id: string) => within(dialog).getAllByRole('option').find((o) => o.getAttribute('data-item') === id);
    expect(row('table:"public"."orders"')).toHaveAttribute('data-changed', 'true');
    expect(row('table:"public"."customers"')).not.toHaveAttribute('data-changed');

    const plan = (await copyStatus(jobId)).plan!;
    expect(plan.acks.map((a) => a.code)).toContain('PLAN_CHANGED');
    const cont = within(dialog).getByRole('button', { name: cs('continue') });
    expect(cont).toBeDisabled();
    for (const a of plan.acks) await user.click(within(dialog).getByRole('checkbox', { name: cs('acknowledgeAria', { code: a.code }) }));
    expect(cont).toBeEnabled();
    await user.click(cont);
    await waitFor(() => expect(jobResume).toHaveBeenCalledWith(jobId, expect.arrayContaining(keysOf(plan))));
    await waitFor(() => expect(['progress', 'result']).toContain(wiz().step));
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.state).toBe('done'), { timeout: 4000 });
  });

  it('missing acknowledgements: a warned job that continued elsewhere moves the wizard on', async () => {
    renderWizard();
    // Every table already exists in staging/orders: skipping them needs an acknowledgement.
    const req = { ...request(), target: { connId: 'c-staging', database: 'orders' } };
    const plan = await copyPlan(req);
    expect(plan.acks.map((a) => a.code)).toContain('TABLES_SKIPPED_EXISTING');
    const jobId = await copyStart(req, plan.planId, []);
    await waitWarned(jobId);
    await useCopyStore.getState().showJob(jobId);
    expect(wiz().step).toBe('plan');
    expect(within(screen.getByRole('dialog')).queryByText(cs('planChangedNotice'))).toBeNull();
    // Another window (or Activity) acknowledges and continues it.
    await jobResume(jobId, keysOf(plan));
    await waitFor(() => expect(['progress', 'result']).toContain(wiz().step));
  });
});
