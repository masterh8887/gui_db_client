// Event names pushed by the Tauri shell (`src-tauri/src/events.rs`) and a single
// `subscribeAll` that wires every one of them. The UI never derives job state itself: it only
// renders what these events say.

import type {
  DriverInfo,
  HostKeyPrompt,
  JobDone,
  JobProgress,
  JobServer,
  JobStateEvent,
  LogEvent,
  SshPasswordPrompt,
} from 'ipc-types';
import { listen } from './ipc';

export const EVENTS = {
  jobState: 'job:state',
  jobProgress: 'job:progress',
  jobServer: 'job:server',
  jobDone: 'job:done',
  jobLog: 'job:log',
  tunnelHostKey: 'tunnel:hostkey',
  tunnelPassword: 'tunnel:password',
  driversChanged: 'drivers:changed',
} as const;

export type EventName = (typeof EVENTS)[keyof typeof EVENTS];

export interface EventPayloads {
  'job:state': JobStateEvent;
  'job:progress': JobProgress;
  'job:server': JobServer;
  'job:done': JobDone;
  'job:log': LogEvent;
  'tunnel:hostkey': HostKeyPrompt;
  'tunnel:password': SshPasswordPrompt;
  'drivers:changed': DriverInfo[];
}

export interface EventHandlers {
  onJobState?: (ev: JobStateEvent) => void;
  onJobProgress?: (ev: JobProgress) => void;
  onJobServer?: (ev: JobServer) => void;
  onJobDone?: (ev: JobDone) => void;
  onJobLog?: (ev: LogEvent) => void;
  onHostKey?: (ev: HostKeyPrompt) => void;
  onSshPassword?: (ev: SshPasswordPrompt) => void;
  onDriversChanged?: (ev: DriverInfo[]) => void;
}

/** Listen to one typed event. */
export function listenTo<E extends EventName>(
  event: E,
  handler: (payload: EventPayloads[E]) => void,
): Promise<() => void> {
  return listen<EventPayloads[E]>(event, handler);
}

/**
 * Attach every `job:*`, `tunnel:*` and `drivers:*` listener at once. Resolves to a function
 * that detaches all of them. Handlers that are not given are simply not subscribed.
 */
export async function subscribeAll(h: EventHandlers): Promise<() => void> {
  const pending: Promise<() => void>[] = [];
  if (h.onJobState) pending.push(listenTo(EVENTS.jobState, h.onJobState));
  if (h.onJobProgress) pending.push(listenTo(EVENTS.jobProgress, h.onJobProgress));
  if (h.onJobServer) pending.push(listenTo(EVENTS.jobServer, h.onJobServer));
  if (h.onJobDone) pending.push(listenTo(EVENTS.jobDone, h.onJobDone));
  if (h.onJobLog) pending.push(listenTo(EVENTS.jobLog, h.onJobLog));
  if (h.onHostKey) pending.push(listenTo(EVENTS.tunnelHostKey, h.onHostKey));
  if (h.onSshPassword) pending.push(listenTo(EVENTS.tunnelPassword, h.onSshPassword));
  if (h.onDriversChanged) pending.push(listenTo(EVENTS.driversChanged, h.onDriversChanged));
  const unlisteners = await Promise.all(pending);
  return () => {
    for (const u of unlisteners) {
      try {
        u();
      } catch {
        // already detached
      }
    }
  };
}
