'use client';

// Script Mode tab (DESIGN.md extension). Layout:
//   ScriptBar (28px top bar: runtime picker, version badge, run / stop button)
//   <textarea> (script editor — plain mono text, grows to fill available space)
//   ScriptLogView (scrollable output: stdout green, stderr red)

import type { ScriptRuntime } from 'ipc-types';
import { Trash2 } from 'lucide-react';
import { useEffect, useRef } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { formatDuration } from '@/lib/format';
import { cn } from '@/lib/utils';
import { Button, DropdownMenu, Spinner, StatusBar, Tooltip, type MenuItem } from '@/components/ui';
import { useScriptStore, type LogLine, type ScriptJobState } from '@/stores/script';
import { ss } from './strings';

const RUNTIMES: ScriptRuntime[] = ['node', 'python', 'bash'];
const RUNTIME_LABELS: Record<ScriptRuntime, string> = { node: ss('runtimeNode'), python: ss('runtimePython'), bash: ss('runtimeBash') };

// ----- RuntimeBadge --------------------------------------------------------------------------

function RuntimeBadge({ runtime }: { runtime: ScriptRuntime }) {
  const info = useScriptStore((s) => s.runtimeInfo[runtime]);
  const store = useScriptStore.getState;

  useEffect(() => {
    if (info === undefined) void store().detectRuntime(runtime);
  }, [runtime, info, store]);

  if (info === undefined) return <span className="text-fg-secondary text-xs">{ss('detecting')}</span>;
  if (info === null) return <span className="text-fg-danger text-xs">{ss('notInstalled')}</span>;
  return <span className="text-fg-success text-xs font-mono">{info.version}</span>;
}

// ----- ScriptBar -----------------------------------------------------------------------------

function ScriptBar({ tabId }: { tabId: string }) {
  const { runtime, state } = useScriptStore(useShallow((s) => ({ runtime: s.docs[tabId]?.runtime ?? 'node', state: s.docs[tabId]?.state ?? 'idle' })));
  const runtimeInfo = useScriptStore((s) => s.runtimeInfo[runtime]);
  const store = useScriptStore.getState;
  const running = state === 'running' || state === 'install';

  const runtimeItems: MenuItem[] = RUNTIMES.map((r) => ({
    id: r,
    label: RUNTIME_LABELS[r],
    checked: runtime === r,
    onSelect: () => {
      store().setRuntime(tabId, r);
      void store().detectRuntime(r);
    },
  }));

  const handleRun = () => {
    if (running) {
      void store().cancel(tabId);
    } else {
      if (runtimeInfo === null) {
        // Not installed: install first
        void store().installRuntime(tabId, runtime);
      } else {
        void store().run(tabId);
      }
    }
  };

  return (
    <StatusBar position="top">
      {/* Runtime picker */}
      <DropdownMenu items={runtimeItems} align="start">
        <button className="flex items-center gap-1 rounded px-1.5 py-0.5 text-sm font-medium text-fg hover:bg-hover active:bg-active">
          {RUNTIME_LABELS[runtime]}
          <span className="text-fg-tertiary">▾</span>
        </button>
      </DropdownMenu>

      {/* Version badge */}
      <RuntimeBadge runtime={runtime} />

      <div className="flex-1" />

      {/* Install / run / stop */}
      {runtimeInfo === null && !running ? (
        <Tooltip content={ss('installPrompt', RUNTIME_LABELS[runtime])}>
          <Button size="sm" variant="secondary" onClick={handleRun}>
            {ss('install')}
          </Button>
        </Tooltip>
      ) : (
        <Button size="sm" variant={running ? 'secondary' : 'primary'} onClick={handleRun} disabled={runtimeInfo === undefined}>
          {running ? (
            <>
              <Spinner size={12} className="mr-1" />
              {ss('stop')}
            </>
          ) : (
            ss('run')
          )}
        </Button>
      )}
    </StatusBar>
  );
}

// ----- ScriptEditor --------------------------------------------------------------------------

function ScriptEditor({ tabId, runtime }: { tabId: string; runtime: ScriptRuntime }) {
  const script = useScriptStore((s) => s.docs[tabId]?.script ?? '');
  const store = useScriptStore.getState;
  const placeholder = runtime === 'node' ? ss('placeholderNode') : runtime === 'python' ? ss('placeholderPython') : ss('placeholderBash');

  return (
    <textarea
      className="min-h-0 flex-1 resize-none bg-bg px-3 py-2 font-mono text-sm text-fg outline-none placeholder:text-fg-placeholder"
      value={script}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(e) => store().setScript(tabId, e.target.value)}
    />
  );
}

// ----- ScriptLogView -------------------------------------------------------------------------

function ScriptFooter({ tabId }: { tabId: string }) {
  const { state, exitCode, durationMs } = useScriptStore(
    useShallow((s) => ({ state: s.docs[tabId]?.state ?? 'idle', exitCode: s.docs[tabId]?.exitCode ?? null, durationMs: s.docs[tabId]?.durationMs ?? null })),
  );
  const store = useScriptStore.getState;

  const stateLabel: Record<ScriptJobState, string> = {
    idle: ss('stateIdle'),
    running: ss('stateRunning'),
    install: ss('stateInstall'),
    done: ss('stateDone'),
    failed: ss('stateFailed'),
    cancelled: ss('stateCancelled'),
  };
  const stateColor: Record<ScriptJobState, string> = {
    idle: 'text-fg-secondary',
    running: 'text-fg-info',
    install: 'text-fg-info',
    done: 'text-fg-success',
    failed: 'text-fg-danger',
    cancelled: 'text-fg-secondary',
  };

  return (
    <StatusBar position="top">
      <span className={cn('text-xs', stateColor[state])}>{stateLabel[state]}</span>
      {exitCode !== null && <span className="font-mono text-xs text-fg-secondary">{`exit ${exitCode}`}</span>}
      {durationMs !== null && <span className="text-xs text-fg-tertiary">{formatDuration(durationMs)}</span>}
      <div className="flex-1" />
      <Tooltip content={ss('clearLog')}>
        <button className="rounded p-0.5 text-fg-secondary hover:text-fg hover:bg-hover active:bg-active" onClick={() => store().clearLog(tabId)}>
          <Trash2 size={13} />
        </button>
      </Tooltip>
    </StatusBar>
  );
}

function LogLineRow({ line }: { line: LogLine }) {
  const isErr = line.stream === 'stderr';
  return (
    <div className={cn('flex gap-2 px-3 py-0 font-mono text-xs leading-5 whitespace-pre-wrap break-all', isErr ? 'text-fg-danger' : 'text-fg')}>
      <span className={cn('shrink-0 select-none', isErr ? 'text-fg-danger' : 'text-fg-tertiary')}>{isErr ? '✕' : '›'}</span>
      <span>{line.text}</span>
    </div>
  );
}

function ScriptLogView({ tabId }: { tabId: string }) {
  const lines = useScriptStore((s) => s.docs[tabId]?.lines ?? []);
  const ref = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom when new lines arrive.
  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length]);

  return (
    <div ref={ref} className="min-h-0 flex-1 overflow-y-auto py-1 bg-bg">
      {lines.map((line, i) => (
        <LogLineRow key={i} line={line} />
      ))}
    </div>
  );
}

// ----- ScriptTabView (root) ------------------------------------------------------------------

export function ScriptTabView({ tabId }: { tabId: string }) {
  const connId = useScriptStore((s) => s.docs[tabId]?.connId ?? '');
  const runtime = useScriptStore((s) => s.docs[tabId]?.runtime ?? 'node');

  if (!connId) return null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <ScriptBar tabId={tabId} />
      <ScriptEditor tabId={tabId} runtime={runtime} />
      <ScriptFooter tabId={tabId} />
      <ScriptLogView tabId={tabId} />
    </div>
  );
}
