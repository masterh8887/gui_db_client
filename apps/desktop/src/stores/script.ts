'use client';

// Script Mode store. Each script tab owns a `ScriptDoc` keyed by tab id. The store handles
// runtime detection, install, run and cancel by calling through to `src/lib/commands.ts`.

import type { RuntimeInfo, ScriptDone, ScriptLine, ScriptRuntime, ScriptStream } from 'ipc-types';
import { create } from 'zustand';
import { scriptCancel, scriptDetectRuntime, scriptInstallRuntime, scriptRun } from '@/lib/commands';
import { listen } from '@/lib/ipc';
import { logLocal } from './logs';

// ----- types ---------------------------------------------------------------------------------

export type ScriptJobState = 'idle' | 'running' | 'install' | 'done' | 'failed' | 'cancelled';

export interface LogLine {
  stream: ScriptStream;
  text: string;
  ts: string;
}

export interface ScriptDoc {
  tabId: string;
  connId: string;
  runtime: ScriptRuntime;
  script: string;
  jobId: string | null;
  state: ScriptJobState;
  exitCode: number | null;
  durationMs: number | null;
  lines: LogLine[];
}

export interface ScriptState {
  docs: Record<string, ScriptDoc>;
  /** Cached probe results, one per runtime. */
  runtimeInfo: Partial<Record<ScriptRuntime, RuntimeInfo | null>>;

  createDoc(tabId: string, connId: string): void;
  disposeDoc(tabId: string): void;
  setRuntime(tabId: string, runtime: ScriptRuntime): void;
  setScript(tabId: string, script: string): void;
  clearLog(tabId: string): void;
  detectRuntime(runtime: ScriptRuntime): Promise<RuntimeInfo | null>;
  installRuntime(tabId: string, runtime: ScriptRuntime): Promise<void>;
  run(tabId: string): Promise<void>;
  cancel(tabId: string): Promise<void>;
}

// ----- listeners setup -----------------------------------------------------------------------

let listenersSetup = false;
// job_id → tabId mapping (for routing events back to the right doc)
const jobToTab: Map<string, string> = new Map();

function ensureListeners() {
  if (listenersSetup) return;
  listenersSetup = true;

  void listen<ScriptLine>('script:line', (line) => {
    const tabId = jobToTab.get(line.jobId);
    if (!tabId) return;
    useScriptStore.setState((s) => {
      const doc = s.docs[tabId];
      if (!doc) return s;
      return { docs: { ...s.docs, [tabId]: { ...doc, lines: [...doc.lines, { stream: line.stream, text: line.text, ts: line.ts }] } } };
    });
  });

  void listen<ScriptDone>('script:done', (done) => {
    const tabId = jobToTab.get(done.jobId);
    if (tabId) jobToTab.delete(done.jobId);
    if (!tabId) return;
    // durationMs is bigint in the generated type (ts-rs renders u64 as bigint) but arrives as a
    // plain JSON number on the wire; coerce to number so the store type stays consistent.
    const durationMs = Number(done.durationMs);
    const exitCode = done.exitCode !== undefined ? done.exitCode : null;
    useScriptStore.setState((s) => {
      const doc = s.docs[tabId];
      if (!doc) return s;
      const state: ScriptJobState = done.cancelled ? 'cancelled' : (exitCode ?? -1) === 0 ? 'done' : 'failed';
      return { docs: { ...s.docs, [tabId]: { ...doc, state, exitCode, durationMs, jobId: null } } };
    });
  });
}

// ----- store ---------------------------------------------------------------------------------

export const useScriptStore = create<ScriptState>()((set, get) => ({
  docs: {},
  runtimeInfo: {},

  createDoc: (tabId, connId) => {
    ensureListeners();
    set((s) => ({
      docs: {
        ...s.docs,
        [tabId]: { tabId, connId, runtime: 'node', script: '', jobId: null, state: 'idle', exitCode: null, durationMs: null, lines: [] },
      },
    }));
  },

  disposeDoc: (tabId) => {
    const doc = get().docs[tabId];
    if (doc?.jobId) jobToTab.delete(doc.jobId);
    set((s) => {
      const docs = { ...s.docs };
      delete docs[tabId];
      return { docs };
    });
  },

  setRuntime: (tabId, runtime) => {
    set((s) => {
      const doc = s.docs[tabId];
      if (!doc) return s;
      return { docs: { ...s.docs, [tabId]: { ...doc, runtime } } };
    });
  },

  setScript: (tabId, script) => {
    set((s) => {
      const doc = s.docs[tabId];
      if (!doc) return s;
      return { docs: { ...s.docs, [tabId]: { ...doc, script } } };
    });
  },

  clearLog: (tabId) => {
    set((s) => {
      const doc = s.docs[tabId];
      if (!doc) return s;
      return { docs: { ...s.docs, [tabId]: { ...doc, lines: [], exitCode: null, durationMs: null, state: 'idle' } } };
    });
  },

  detectRuntime: async (runtime) => {
    const existing = get().runtimeInfo[runtime];
    if (existing !== undefined) return existing;
    try {
      const info = await scriptDetectRuntime(runtime);
      set((s) => ({ runtimeInfo: { ...s.runtimeInfo, [runtime]: info } }));
      return info;
    } catch (e) {
      logLocal('error', `scriptDetectRuntime(${runtime}): ${String(e)}`);
      set((s) => ({ runtimeInfo: { ...s.runtimeInfo, [runtime]: null } }));
      return null;
    }
  },

  installRuntime: async (tabId, runtime) => {
    const doc = get().docs[tabId];
    if (!doc) return;
    try {
      const jobId = await scriptInstallRuntime(runtime);
      jobToTab.set(jobId, tabId);
      set((s) => {
        const d = s.docs[tabId];
        if (!d) return s;
        return { docs: { ...s.docs, [tabId]: { ...d, jobId, state: 'install', lines: [] } } };
      });
    } catch (e) {
      logLocal('error', `scriptInstallRuntime(${runtime}): ${String(e)}`);
    }
  },

  run: async (tabId) => {
    const doc = get().docs[tabId];
    if (!doc || doc.state === 'running' || doc.state === 'install') return;
    const jobId = crypto.randomUUID();
    jobToTab.set(jobId, tabId);
    set((s) => {
      const d = s.docs[tabId];
      if (!d) return s;
      return { docs: { ...s.docs, [tabId]: { ...d, jobId, state: 'running', exitCode: null, durationMs: null, lines: [] } } };
    });
    try {
      await scriptRun(jobId, doc.connId, doc.runtime, doc.script);
    } catch (e) {
      jobToTab.delete(jobId);
      set((s) => {
        const d = s.docs[tabId];
        if (!d) return s;
        const errLine: LogLine = { stream: 'stderr', text: `IPC error: ${String(e)}`, ts: new Date().toISOString() };
        return { docs: { ...s.docs, [tabId]: { ...d, jobId: null, state: 'failed', lines: [...d.lines, errLine] } } };
      });
    }
  },

  cancel: async (tabId) => {
    const doc = get().docs[tabId];
    if (!doc?.jobId) return;
    try {
      await scriptCancel(doc.jobId);
    } catch (e) {
      logLocal('error', `scriptCancel: ${String(e)}`);
    }
  },
}));
