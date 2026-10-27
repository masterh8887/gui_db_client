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
