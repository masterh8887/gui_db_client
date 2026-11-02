'use client';

// Console drawer content (DESIGN.md §5.G); the frame (full width at the window bottom,
// resizable, `ui.logHeight`) is layout's ConsoleFrame. Header 28px: 콘솔, count, level, clear,
// close. Rows mono 12/18: time · LEVEL · [jobId] · target (from 1280px) · message. It sticks to
// the bottom unless the user scrolled up. Auto-open on error lives in the logs store.

import type { LogLevel } from 'ipc-types';
import { Trash2, X } from 'lucide-react';
import { useLayoutEffect, useRef } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { formatTime } from '@/lib/format';
import { cn } from '@/lib/utils';
import { escapeIsLocal, IconButton, Select, StatusBar } from '@/components/ui';
import { selectVisibleLogs, useLogsStore } from '@/stores/logs';
import { cs } from './strings';

const LEVEL_CLASS: Record<LogLevel, string> = {
  trace: 'text-fg-tertiary',
  debug: 'text-fg-tertiary',
  info: 'text-fg-secondary',
  warn: 'text-warning',
  error: 'text-danger',
};

export const LOG_LEVELS: readonly LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

/** Distance from the bottom (px) that still counts as "at the bottom". */
const STICK_SLACK = 24;

export function ConsoleDrawer({ onClose }: { onClose: () => void }) {
  const entries = useLogsStore(useShallow(selectVisibleLogs));
  const errorCount = useLogsStore((s) => s.errorCount);
  const filterLevel = useLogsStore((s) => s.filterLevel);
  const setFilter = useLogsStore((s) => s.setFilter);
  const clear = useLogsStore((s) => s.clear);
  const listRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  useLayoutEffect(() => {
    const el = listRef.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [entries]);

  const onScroll = () => {
    const el = listRef.current;
    if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < STICK_SLACK;
  };

  return (
    <div
      className="flex h-full min-h-0 flex-col"
      onKeyDown={(e) => {
        // Esc inside the drawer closes it (consumed, so it never asks to cancel a job). Menus
        // portalled out of the drawer (the level Select) keep their own Esc.
        if (e.key !== 'Escape' || e.defaultPrevented || escapeIsLocal(e.nativeEvent)) return;
        if (!(e.target instanceof Node) || !e.currentTarget.contains(e.target)) return;
        e.preventDefault();
        onClose();
      }}
    >
      <StatusBar position="top" className="gap-1.5 pr-1">
        <span className="font-semibold text-fg-secondary">{cs('title')}</span>
        <span className="text-xs tabular-nums text-fg-tertiary" aria-label={cs('count', { n: entries.length })}>
          {entries.length}
        </span>
        {errorCount > 0 && <span className="text-danger">{cs('errors', { n: errorCount })}</span>}
        <div className="ml-auto flex items-center gap-1">
          <Select<LogLevel>
            size="sm"
            aria-label={cs('level')}
            className="w-[88px]"
            value={filterLevel}
            onChange={setFilter}
            options={LOG_LEVELS.map((l) => ({ value: l, label: l }))}
          />
          <IconButton label={cs('clear')} size="sm" onClick={clear} disabled={entries.length === 0 && errorCount === 0}>
            <Trash2 size={14} strokeWidth={1.75} />
          </IconButton>
          <IconButton label={cs('close')} size="sm" onClick={onClose}>
            <X size={14} strokeWidth={1.75} />
          </IconButton>
        </div>
      </StatusBar>
      <div
        ref={listRef}
        role="log"
        aria-label={cs('title')}
        tabIndex={0}
        data-selectable
        onScroll={onScroll}
        className="list-scroll min-h-0 flex-1 py-1 font-mono text-sm leading-[18px] outline-none focus-visible:shadow-[inset_0_0_0_2px_var(--focus)]"
      >
        {entries.length === 0 ? (
          <div className="px-3 py-1 font-sans text-fg-secondary">{cs('empty')}</div>
        ) : (
          entries.map((e, i) => (
            <div
              key={`${e.at}-${i}`}
              data-level={e.level}
              className={cn('flex gap-2 px-3 whitespace-pre-wrap', e.level === 'error' && 'bg-danger-soft')}
            >
              <span className="shrink-0 text-fg-tertiary">{formatTime(e.at)}</span>
              <span className={cn('w-11 shrink-0 uppercase', LEVEL_CLASS[e.level])}>{e.level}</span>
              {e.jobId && <span className="shrink-0 text-fg-tertiary">[{e.jobId}]</span>}
              {e.target && <span className="hidden shrink-0 text-fg-tertiary min-[1280px]:inline">{e.target}</span>}
              <span className="min-w-0 flex-1 break-words">{e.message}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
