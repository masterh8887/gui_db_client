'use client';

// Subscribes once to every backend event and dispatches into the stores (DESIGN.md §7.6). Also
// bootstraps the stores on mount, including `job_list` so jobs that were running before a
// restart re-attach, and routes the macOS menu (`menu:action`) to `runAction`. Mounted by
// AppShell outside the `key={lang}` subtree: it never re-subscribes or re-bootstraps.

import type { JobState } from 'ipc-types';
import { useEffect } from 'react';
import { runAction } from '@/lib/actions';
import { subscribeAll, subscribeWindow } from '@/lib/events';
import { errorText } from '@/lib/ipc-error';
import { toast } from '@/lib/toast';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { logLocal, useLogsStore } from '@/stores/logs';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';
import { useWindowsStore } from '@/stores/windows';
import { es } from './AppEvents.strings';

/** Event the Tauri shell emits for custom macOS menu items; the payload is the action id. */
export const MENU_ACTION_EVENT = 'menu:action';

/** The action id of a `menu:action` payload (a plain id, or `{ id }` / `{ action }`). */
export function menuActionId(payload: unknown): string | null {
  if (typeof payload === 'string') return payload || null;
  if (payload && typeof payload === 'object') {
    const p = payload as { id?: unknown; action?: unknown };
    if (typeof p.id === 'string' && p.id) return p.id;
    if (typeof p.action === 'string' && p.action) return p.action;
  }
  return null;
}

/**
 * Completion toast for a copy / export the window that started it holds the claim for (db-copy.md
 * 1.1): a job that finished while its panel was closed still gives notice. Tone follows the end
 * state. Other kinds (e.g. exact count) are routed elsewhere and never toast here.
 */
function notifyJobDone(kind: 'copy' | 'export', state: JobState, label: string): void {
  const copy = kind === 'copy';
  if (state === 'done') toast(copy ? es('copyComplete', { label }) : es('exportComplete', { label }), { tone: 'success' });
  else if (state === 'failed' || state === 'rejected') toast(copy ? es('copyFailed', { label }) : es('exportFailed', { label }), { tone: 'danger' });
  else if (state === 'cancelled') toast(copy ? es('copyCancelled', { label }) : es('exportCancelled', { label }));
}

async function bootstrap(): Promise<void> {
  const loads = [
    ['connections', () => useConnectionsStore.getState().load()],
    ['settings', () => useSettingsStore.getState().load()],
    ['logs', () => useLogsStore.getState().loadRecent()],
    ['jobs', () => useJobsStore.getState().load()],
  ] as const;
  const results = await Promise.allSettled(loads.map(([, run]) => run()));
  results.forEach((r, i) => {
    if (r.status === 'rejected') logLocal('error', es('loadFailed', { what: es(loads[i][0]), error: errorText(r.reason) }));
  });
}

/** Mount once near the root. Renders nothing. */
export function AppEvents() {
  useEffect(() => {
    let disposed = false;
    const unsubscribers: (() => void)[] = [];
    const keep = (u: () => void) => {
      if (disposed) u();
      else unsubscribers.push(u);
    };

    // `conn_save` emits `connections:changed` on every save, and a DBeaver import fires one per
    // row. Coalesce them into a single trailing reload (one per ~100 ms) so N imported rows do
    // not start N overlapping `load()` calls per window; `load()` itself drops stale responses.
    let connReloadTimer: ReturnType<typeof setTimeout> | null = null;
    const reloadConnectionsSoon = () => {
      if (connReloadTimer) return;
      connReloadTimer = setTimeout(() => {
        connReloadTimer = null;
        void useConnectionsStore.getState().load();
      }, 100);
    };
    keep(() => {
      if (connReloadTimer) clearTimeout(connReloadTimer);
      connReloadTimer = null;
    });

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
        // Completion toast only in the window holding the claim (db-copy.md 1.1), for copy /
        // export jobs; counts and other kinds are surfaced in their own places.
        if (jobs.local[d.jobId]) {
          const done = useJobsStore.getState().jobs[d.jobId]?.summary;
          if (done && (done.kind === 'copy' || done.kind === 'export')) notifyJobDone(done.kind, done.state, done.label);
        }
        const ui = useUiStore.getState();
        if (ui.cancelConfirmJobId === d.jobId) ui.setCancelConfirm(null);
        // A copy wrote into its target: drop that connection's cached catalog in every window.
        const copyTarget = before?.summary.copy?.targetConnId;
        if (copyTarget) useCatalogStore.getState().clearConnection(copyTarget);
        const finishUp = (connId: string | undefined, kind: string | undefined) => {
          if (d.exactCount && connId) useCatalogStore.getState().setExactCount(connId, d.exactCount);
          // A count that failed or was cancelled must not leave the "counting" spinner behind.
          else useCatalogStore.getState().clearCountingJob(d.jobId);
          // Every history page loaded so far (sidebar 기록, history tab, palette) gets the new row.
          if (kind !== 'exact_count') void useHistoryStore.getState().reloadLoaded();
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
      onDriversChanged: (d) => useConnectionsStore.getState().setDrivers(d),
      // Another window or BulkDB process saved connections / settings (db-copy.md 1.2).
      onConnectionsChanged: () => reloadConnectionsSoon(),
      onSettingsChanged: () => void useSettingsStore.getState().load(),
      onWindowsChanged: (ws) => useWindowsStore.getState().apply(ws),
      // A tunnel prompt was answered (maybe in another window), timed out or dropped.
      onPromptResolved: ({ promptId }) => {
        const ui = useUiStore.getState();
        ui.resolveHostKeyPrompt(promptId);
        ui.resolveSshPasswordPrompt(promptId);
      },
    }).then(keep);

    // Window-targeted events: only this window's (`listenWindow`, db-copy.md 1.1). The fifth one,
    // `app:quit-requested`, belongs to `initWindows()` (stores/windows.ts), which asks once.
    void subscribeWindow({
      // macOS menu bar items (core's custom menu); a disabled or unknown action is a no-op.
      onMenuAction: (payload) => {
        const id = menuActionId(payload);
        if (id) void runAction(id);
      },
      onHostKey: (p) => useUiStore.getState().pushHostKeyPrompt(p),
      onSshPassword: (p) => useUiStore.getState().pushSshPasswordPrompt(p),
      onJobsClaimed: ({ jobIds }) => useJobsStore.getState().addLocal(jobIds),
    }).then(keep);

    void bootstrap();

    return () => {
      disposed = true;
      for (const u of unsubscribers.splice(0)) u();
    };
  }, []);
  return null;
}
