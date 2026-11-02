// Presentation of the nine job states, shared by every panel that shows a job (monitor cards,
// history rows). The states themselves come from Rust (`JobState`); nothing here derives one.

import type { JobState } from 'ipc-types';

/** Badge tone per state (a subset of the `Badge` tones). */
export type StateTone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger';

export const STATE_TONE: Record<JobState, StateTone> = {
  pending: 'neutral',
  preflight: 'accent',
  warned: 'warning',
  rejected: 'danger',
  running: 'accent',
  finishing: 'accent',
  done: 'success',
  cancelled: 'warning',
  failed: 'danger',
};

/** States during which the backend is actively working (the card shows a spinner). */
export function isBusyState(state: JobState): boolean {
  return state === 'pending' || state === 'preflight' || state === 'running' || state === 'finishing';
}

/**
 * Progress of a running job in percent, derived from elapsed time and the ETA (the backend
 * reports no row total): `undefined` while the ETA is unknown (show an indeterminate bar).
 */
export function jobProgressPercent(p: { elapsedMs: bigint | number; etaSec?: bigint | number } | undefined): number | undefined {
  if (!p || p.etaSec === undefined || p.etaSec === null) return undefined;
  const elapsed = Number(p.elapsedMs);
  const remaining = Number(p.etaSec) * 1000;
  if (!Number.isFinite(elapsed) || !Number.isFinite(remaining) || elapsed + remaining <= 0) return undefined;
  return Math.max(0, Math.min(100, (elapsed / (elapsed + remaining)) * 100));
}
