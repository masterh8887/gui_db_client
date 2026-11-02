// Event names pushed by the Tauri shell (`src-tauri/src/events.rs`) and a single
// `subscribeAll` that wires every one of them. The UI never derives job state itself: it only
// renders what these events say.

import type {
  CopyProgress,
  DriverInfo,
  HostKeyPrompt,
  JobDone,
  JobProgress,
  JobServer,
  JobStateEvent,
  LogEvent,
  PromptResolved,
  Settings,
  SshPasswordPrompt,
  WindowInfo,
} from 'ipc-types';
import { listen, listenWindow } from './ipc';

export const EVENTS = {
  jobState: 'job:state',
  jobProgress: 'job:progress',
  jobServer: 'job:server',
  jobDone: 'job:done',
  jobLog: 'job:log',
  tunnelHostKey: 'tunnel:hostkey',
  tunnelPassword: 'tunnel:password',
  driversChanged: 'drivers:changed',
  copyProgress: 'copy:progress',
  windowsChanged: 'windows:changed',
  connectionsChanged: 'connections:changed',
  settingsChanged: 'settings:changed',
  promptResolved: 'prompt:resolved',
} as const;

/**
 * Window-targeted events (docs/db-copy.md 1.1): sent with `emit_to(<label>)` and subscribed only
 * through `listenWindow` (never `listen`, which would also receive other windows' ones).
 */
export const WINDOW_EVENTS = {
  menuAction: 'menu:action',
  tunnelHostKey: 'tunnel:hostkey',
  tunnelPassword: 'tunnel:password',
  jobsClaimed: 'jobs:claimed',
  appQuitRequested: 'app:quit-requested',
} as const;

/** `jobs:claimed`: jobs of a closed window handed to this one. */
export interface JobsClaimed {
  jobIds: string[];
}

/** `app:quit-requested`: the OS / menu asked to quit while jobs run. */
export interface AppQuitRequested {
  jobs: number;
}

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
  'copy:progress': CopyProgress;
  'windows:changed': WindowInfo[];
  'connections:changed': null;
  'settings:changed': Settings;
  'prompt:resolved': PromptResolved;
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
  onCopyProgress?: (ev: CopyProgress) => void;
  onWindowsChanged?: (ev: WindowInfo[]) => void;
  onConnectionsChanged?: () => void;
  onSettingsChanged?: (ev: Settings) => void;
  onPromptResolved?: (ev: PromptResolved) => void;
}

/** Handlers of the window-targeted events (`subscribeWindow`). */
export interface WindowEventHandlers {
  onMenuAction?: (payload: unknown) => void;
  onHostKey?: (ev: HostKeyPrompt) => void;
  onSshPassword?: (ev: SshPasswordPrompt) => void;
  onJobsClaimed?: (ev: JobsClaimed) => void;
  onAppQuitRequested?: (ev: AppQuitRequested) => void;
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
  if (h.onCopyProgress) pending.push(listenTo(EVENTS.copyProgress, h.onCopyProgress));
  if (h.onWindowsChanged) pending.push(listenTo(EVENTS.windowsChanged, h.onWindowsChanged));
  const onConnectionsChanged = h.onConnectionsChanged;
  if (onConnectionsChanged) pending.push(listenTo(EVENTS.connectionsChanged, () => onConnectionsChanged()));
  if (h.onSettingsChanged) pending.push(listenTo(EVENTS.settingsChanged, h.onSettingsChanged));
  if (h.onPromptResolved) pending.push(listenTo(EVENTS.promptResolved, h.onPromptResolved));
  return detachAll(await Promise.all(pending));
}

/**
 * Attach the window-targeted listeners (`listenWindow`: this window only). Resolves to a
 * function that detaches all of them.
 */
export async function subscribeWindow(h: WindowEventHandlers): Promise<() => void> {
  const pending: Promise<() => void>[] = [];
  if (h.onMenuAction) pending.push(listenWindow<unknown>(WINDOW_EVENTS.menuAction, h.onMenuAction));
  if (h.onHostKey) pending.push(listenWindow<HostKeyPrompt>(WINDOW_EVENTS.tunnelHostKey, h.onHostKey));
  if (h.onSshPassword) pending.push(listenWindow<SshPasswordPrompt>(WINDOW_EVENTS.tunnelPassword, h.onSshPassword));
  if (h.onJobsClaimed) pending.push(listenWindow<JobsClaimed>(WINDOW_EVENTS.jobsClaimed, h.onJobsClaimed));
  if (h.onAppQuitRequested) pending.push(listenWindow<AppQuitRequested>(WINDOW_EVENTS.appQuitRequested, h.onAppQuitRequested));
  return detachAll(await Promise.all(pending));
}

function detachAll(unlisteners: (() => void)[]): () => void {
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
