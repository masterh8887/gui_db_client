'use client';

// Job actions (DESIGN.md §7.5). The UI never changes job state: these only open panels, ask
// for confirmation and send the request; the `job:*` events decide what happens.

import { useCopyStore } from '@/stores/copy';
import { isActiveState, selectAllJobs, useJobsStore } from '@/stores/jobs';
import { useExtractStore } from '@/stores/extract';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { errorText } from './ipc-error';
import { ls } from './strings';

/**
 * Show the activity list without moving focus: in a workspace the inspector on 활동 (opened
 * only when closed), on Welcome the activity popover.
 */
export function showActivity(): void {
  const ui = useUiStore.getState();
  if (useWorkspaceStore.getState().mode === 'workspace') {
    if (!ui.inspectorOpen) ui.setInspectorOpen(true);
    if (ui.inspectorSegment !== 'activity') ui.setInspectorSegment('activity');
  } else if (!ui.activityPopoverOpen) {
    ui.setActivityPopoverOpen(true);
  }
}

/** Show the inline "cancel this job?" confirmation for `jobId`. */
export function askCancel(jobId: string): void {
  useUiStore.getState().setCancelConfirm(jobId);
  showActivity();
}

/** Esc / Mod+.: dismiss an open cancel confirmation, else ask for the newest active job. */
export function askCancelLatest(): void {
  const ui = useUiStore.getState();
  if (ui.cancelConfirmJobId) {
    ui.setCancelConfirm(null);
    return;
  }
  const active = selectAllJobs(useJobsStore.getState()).filter((e) => isActiveState(e.summary.state));
  const newest = active[active.length - 1];
  if (newest) askCancel(newest.summary.id);
}

/** Send the cancel request (the confirmation closes first); failures are logged. */
export async function cancelJob(jobId: string): Promise<void> {
  useUiStore.getState().setCancelConfirm(null);
  try {
    await useJobsStore.getState().cancel(jobId);
  } catch (e) {
    logLocal('error', ls('cancelFailed', { error: errorText(e) }), jobId);
  }
}

/**
 * Open the resume sheet of a warned job (fetching its preflight report when the store does not
 * have it). Resolves whether the sheet opened.
 */
export async function reviewWarnings(jobId: string): Promise<boolean> {
  // A warned copy job is reviewed in the copy wizard's plan step, never in the export sheet.
  if (useJobsStore.getState().jobs[jobId]?.summary.kind === 'copy') {
    await useCopyStore.getState().showJob(jobId);
    return true;
  }
  let report = useJobsStore.getState().jobs[jobId]?.summary.preflight;
  if (!report) report = (await useJobsStore.getState().refreshJob(jobId))?.preflight;
  if (!report) {
    logLocal('error', ls('reportUnavailable'), jobId);
    return false;
  }
  useExtractStore.getState().openResumeDialog(jobId, report);
  return true;
}
