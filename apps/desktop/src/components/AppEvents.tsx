'use client';

// Subscribes once to every backend event and dispatches into the stores. Also bootstraps the
// stores on mount, including `job_list` so jobs that were running before a restart re-attach.

import { useEffect } from 'react';
import { subscribeAll } from '@/lib/events';
import { errorText } from '@/lib/ipc-error';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { logLocal, useLogsStore } from '@/stores/logs';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';

async function bootstrap(): Promise<void> {
  const results = await Promise.allSettled([
    useConnectionsStore.getState().load(),
    useSettingsStore.getState().load(),
    useLogsStore.getState().loadRecent(),
    useJobsStore.getState().load(),
    useHistoryStore.getState().load(),
  ]);
  const names = ['connections', 'settings', 'logs', 'jobs', 'history'];
  results.forEach((r, i) => {
    if (r.status === 'rejected') logLocal('error', `Failed to load ${names[i]}: ${errorText(r.reason)}`);
  });
  // The extract form may have taken its defaults before the settings arrived.
  const defaultDir = useSettingsStore.getState().settings?.defaultOutputDir;
  const extract = useExtractStore.getState();
  if (defaultDir && !extract.output.dir) extract.setOutput({ dir: defaultDir });
}

/** Mount once near the root. Renders nothing. */
export function AppEvents() {
  useEffect(() => {
    let disposed = false;
    let unsubscribe: (() => void) | undefined;

    void subscribeAll({
      onJobState: (ev) => {
        // `refetch`: the job was unknown, or it entered `warned` and its preflight report (needed
        // to acknowledge the warnings) only comes with `job_get`.
        const { refetch } = useJobsStore.getState().applyState(ev);
        if (refetch) void useJobsStore.getState().refreshJob(ev.jobId);
      },
      onJobProgress: (p) => useJobsStore.getState().applyProgress(p),
      onJobServer: (s) => useJobsStore.getState().applyServer(s),
      onJobDone: (d) => {
        const jobs = useJobsStore.getState();
        const before = jobs.jobs[d.jobId];
        jobs.applyDone(d);
        const ui = useUiStore.getState();
        if (ui.cancelConfirmJobId === d.jobId) ui.setCancelConfirm(null);
        const finishUp = (connId: string | undefined, kind: string | undefined) => {
          if (d.exactCount && connId) useCatalogStore.getState().setExactCount(connId, d.exactCount);
          // A count that failed or was cancelled must not leave the "counting" spinner behind.
          else useCatalogStore.getState().clearCountingJob(d.jobId);
          if (kind !== 'exact_count') void useHistoryStore.getState().load();
        };
        // A job first seen through `job:state` is a stub (no connection, guessed kind) until its
        // `job_get` lands; treat it like an unknown job so a count still reaches its table.
        const known = before?.summary.connId ? before.summary : undefined;
        if (known) finishUp(known.connId, known.kind);
        else void jobs.refreshJob(d.jobId).then((s) => finishUp(s?.connId, s?.kind));
      },
      onJobLog: (l) => {
        useLogsStore.getState().push(l);
        useJobsStore.getState().applyLog(l);
      },
      onHostKey: (p) => useUiStore.getState().pushHostKeyPrompt(p),
      onSshPassword: (p) => useUiStore.getState().pushSshPasswordPrompt(p),
      onDriversChanged: (d) => useConnectionsStore.getState().setDrivers(d),
    }).then((u) => {
      if (disposed) u();
      else unsubscribe = u;
    });

    void bootstrap();

    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, []);
  return null;
}
