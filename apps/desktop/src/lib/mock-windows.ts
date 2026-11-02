// In-browser fake of the window / instance commands (docs/db-copy.md 1). Owner: windows.
//
// A browser tab plays one BulkDB window. `window_new` opens another tab with the window's label
// in the URL (`?window=w-3&index=2&conn=<connId>`); the tabs of one browser find each other on a
// `BroadcastChannel`, so every tab's `windows:changed` lists them all with their connections, the
// Welcome window chips work, and a chip click asks that tab to focus itself (best effort: browsers
// may refuse to raise another tab). Under test (no real tabs) a new window is simulated in the
// registry as if it had opened its connection. `instance_spawn` adds a simulated other BulkDB
// process to `instances_info`, so the "다른 BulkDB n개 실행 중" chip can be seen.
// `window_close_begin` answers like the shell (`last` when every window it knows is closing); a
// tab only knows that it is closing itself, and counts the active jobs windows claimed.
//
// Window-targeted events of this tab use the mock's per-window bus `main` (what `listenWindow`
// subscribes to in mock mode), whatever label the tab plays.

import type { InstanceInfo, InstancesInfo, JobState, WindowInfo, WindowInit } from 'ipc-types';
import type { MockArgs, MockCtx, MockFeature } from './mock-ipc';

/** Label of the first window (and of every tab opened without `?window=`). */
export const MOCK_WINDOW_LABEL = 'main';
/** Window title on Welcome. */
const APP_TITLE = 'BulkDB';
/** Most windows one process shows (as the shell). */
export const MOCK_MAX_WINDOWS = 8;
const CHANNEL = 'bulkdb-mock-windows';
/** Job states the shell counts as active (`JobState::is_active`). */
const ACTIVE_JOB_STATES: readonly JobState[] = ['pending', 'preflight', 'warned', 'running', 'finishing'];

type Msg =
  | { type: 'hello'; info: WindowInfo; maxN?: number }
  | { type: 'state'; info: WindowInfo; maxN?: number }
  | { type: 'bye'; label: string }
  | { type: 'focus'; label: string };

/** Number in a `w-<n>` label; `main` (and anything non-`w-`) counts as window 1. */
function labelNum(label: string): number {
  const n = label.startsWith('w-') ? Number(label.slice(2)) : 1;
  return Number.isFinite(n) ? n : 1;
}

/**
 * Monotonic `w-<n>` allocator shared by the tabs of one browser. It never hands out a number
 * twice, even after the highest-numbered window closes (db-copy.md 1.1: "n counts up, never
 * reused"), matching the shell's monotonic `next_n`. Each tab carries its counter in the
 * hello/state messages, so a tab that opens a window learns the numbers other tabs already used
 * and two tabs opening at once do not collide on the same label.
 */
export function createWindowLabels(selfLabel: string) {
  let maxN = labelNum(selfLabel);
  return {
    /** Raise the counter to cover a label learned from another tab (never lowers it). */
    observe(label: string): void {
      maxN = Math.max(maxN, labelNum(label));
    },
    /** Raise the counter to a peer tab's reported value. */
    raiseTo(n: number | undefined): void {
      if (typeof n === 'number' && Number.isFinite(n)) maxN = Math.max(maxN, n);
    },
    /** The next never-before-used label. */
    next(): string {
      maxN += 1;
      return `w-${maxN}`;
    },
    get maxN(): number {
      return maxN;
    },
  };
}

/** `BulkDB` on Welcome, `BulkDB — <name>` for a workspace (as the shell). */
export function mockWindowTitle(connName?: string): string {
  return connName ? `${APP_TITLE} — ${connName}` : APP_TITLE;
}

/** This tab's window from its URL (`?window=`, `?index=`, `?conn=`). */
function selfFromUrl(): { label: string; index: number; openConnId?: string } {
  const params = typeof location === 'undefined' ? new URLSearchParams() : new URLSearchParams(location.search);
  const label = params.get('window') || MOCK_WINDOW_LABEL;
  const index = Number(params.get('index')) || 1;
  const conn = params.get('conn') || undefined;
  return { label, index, ...(conn ? { openConnId: conn } : {}) };
}

/** Real tabs only in a browser, never under test (jsdom has no `window.open`). */
function realTabs(): boolean {
  return typeof window !== 'undefined' && typeof BroadcastChannel !== 'undefined' && process.env.NODE_ENV !== 'test';
}

export function createWindowHandlers(ctx: MockCtx): MockFeature {
  let url = selfFromUrl();
  let selfLabel = url.label;
  let pendingConn = url.openConnId;
  let windows = new Map<string, WindowInfo>();
  let others: InstanceInfo[] = [];
  const claimed = new Set<string>();
  /** Windows whose page accepted a close (`window_close_begin`) and that are still listed. */
  const closing = new Set<string>();
  let channel: BroadcastChannel | null = null;

  const fresh = (): WindowInfo => ({ label: selfLabel, title: APP_TITLE, focused: true, index: url.index });
  windows.set(selfLabel, fresh());

  const self = (): WindowInfo => windows.get(selfLabel) ?? fresh();
  const list = (): WindowInfo[] => [...windows.values()].sort((a, b) => a.index - b.index);
  const changed = () => ctx.emit('windows:changed', list());
  const post = (m: Msg) => channel?.postMessage(m);
  const smallestFreeIndex = () => {
    const used = new Set([...windows.values()].map((w) => w.index));
    let i = 1;
    while (used.has(i)) i += 1;
    return i;
  };
  let labels = createWindowLabels(selfLabel);

  if (realTabs()) {
    channel = new BroadcastChannel(CHANNEL);
    channel.onmessage = (e: MessageEvent<Msg>) => {
      const m = e.data;
      if (m.type === 'hello' || m.type === 'state') {
        if (m.info.label === selfLabel) return;
        windows.set(m.info.label, { ...m.info, focused: false });
        // Learn the label (and the peer's counter) so this tab never reuses a number.
        labels.observe(m.info.label);
        labels.raiseTo(m.maxN);
        if (m.type === 'hello') post({ type: 'state', info: self(), maxN: labels.maxN });
        changed();
      } else if (m.type === 'bye') {
        if (windows.delete(m.label)) changed();
      } else if (m.label === selfLabel) {
        window.focus();
      }
    };
    post({ type: 'hello', info: self(), maxN: labels.maxN });
    // `pagehide` also fires where `beforeunload` is skipped (a tab closed by the browser or
    // put into the back/forward cache), so a closed tab does not linger as a "창 N" chip.
    window.addEventListener('pagehide', () => post({ type: 'bye', label: selfLabel }));
    window.addEventListener('pageshow', (e) => {
      if (e.persisted) post({ type: 'hello', info: self(), maxN: labels.maxN });
    });
    window.addEventListener('focus', () => {
      for (const [l, w] of windows) windows.set(l, { ...w, focused: l === selfLabel });
      changed();
    });
  }

  const handlers: MockFeature['handlers'] = {
    window_init: (): WindowInit => {
      const s = self();
      const init: WindowInit = { label: s.label, index: s.index, ...(pendingConn ? { openConnId: pendingConn } : {}) };
      pendingConn = undefined;
      return init;
    },
    window_new: (a: MockArgs): string => {
      const connId = typeof a.connId === 'string' && a.connId ? a.connId : undefined;
      const conn = connId ? ctx.findConn(connId) : undefined;
      if (windows.size >= MOCK_MAX_WINDOWS) throw ctx.error('INVALID', '창은 최대 8개까지 열 수 있습니다 (at most 8 windows)');
      const label = labels.next();
      const index = smallestFreeIndex();
      if (channel) {
        const q = new URLSearchParams({ window: label, index: String(index), ...(connId ? { conn: connId } : {}) });
        const opened = window.open(`${location.pathname}?${q.toString()}`, '_blank');
        if (opened) {
          // The new tab announces itself (`hello`); until then it is listed on Welcome.
          windows.set(label, { label, title: APP_TITLE, focused: false, index });
        } else {
          // A popup blocker refused the tab: no placeholder window that would never arrive.
          ctx.log('warn', `window_new: 브라우저가 새 탭을 막았습니다 (popup blocked); ${label} not opened`, undefined, 'bulkdb::mock');
        }
      } else {
        // Simulated: the window opened and showed its connection.
        windows.set(label, {
          label,
          index,
          focused: false,
          title: mockWindowTitle(conn?.name),
          ...(conn ? { connId: conn.id, connName: conn.name } : {}),
        });
      }
      ctx.log('info', `window_new → ${label}${connId ? ` (${connId})` : ''}`, undefined, 'bulkdb::mock');
      changed();
      return label;
    },
    window_list: (): WindowInfo[] => list(),
    window_focus: (a: MockArgs) => {
      const label = String(a.label);
      if (!windows.has(label)) throw ctx.error('NOT_FOUND', `window ${label} not found`);
      if (label === selfLabel) return;
      post({ type: 'focus', label });
      for (const [l, w] of windows) windows.set(l, { ...w, focused: l === label });
      changed();
    },
    window_set_connection: (a: MockArgs) => {
      const connId = typeof a.connId === 'string' && a.connId ? a.connId : undefined;
      const { connId: _c, connName: _n, ...base } = self();
      void _c;
      void _n;
      const conn = connId ? ctx.findConn(connId) : undefined;
      const next: WindowInfo = { ...base, title: mockWindowTitle(conn?.name), ...(conn ? { connId: conn.id, connName: conn.name } : {}) };
      windows.set(selfLabel, next);
      post({ type: 'state', info: next });
      changed();
    },
    window_claim_job: (a: MockArgs) => {
      if (typeof a.jobId === 'string') claimed.add(a.jobId);
    },
    window_ready: () => undefined,
    window_close_begin: (): { last: boolean; jobs: number } => {
      closing.add(selfLabel);
      const last = [...windows.keys()].every((l) => closing.has(l));
      const jobs = [...claimed].filter((id) => {
        const state = ctx.jobs.get(id)?.state;
        return state !== undefined && ACTIVE_JOB_STATES.includes(state);
      }).length;
      return { last, jobs };
    },
    window_close_cancel: () => {
      closing.delete(selfLabel);
    },
    instances_info: (): InstancesInfo => ({ selfPid: 1, others: [...others] }),
    instance_spawn: () => {
      const pid = 2 + others.length;
      others = [...others, { pid, instanceId: `mock-instance-${pid}`, kind: 'desktop', startedAt: ctx.nowIso(), version: '0.1.0-mock' }];
      ctx.log('info', `instance_spawn: simulated another BulkDB (pid ${pid})`, undefined, 'bulkdb::mock');
    },
    app_quit_confirmed: () => {
      ctx.log('info', 'app_quit_confirmed: the browser mock does not quit', undefined, 'bulkdb::mock');
    },
  };

  return {
    handlers,
    reset() {
      // Tests set the URL (`?window=&index=&conn=`) before a reset to play another window.
      url = selfFromUrl();
      selfLabel = url.label;
      pendingConn = url.openConnId;
      windows = new Map([[selfLabel, fresh()]]);
      labels = createWindowLabels(selfLabel);
      others = [];
      claimed.clear();
      closing.clear();
    },
  };
}
