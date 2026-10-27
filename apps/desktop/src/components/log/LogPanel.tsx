'use client';

// Bottom panel. Collapsed it is a 28px bar; it auto-expands when an error log arrives
// (the logs store flips `ui.logOpen`).

import type { LogLevel } from 'ipc-types';
import { ChevronDown, ChevronUp, Trash2 } from 'lucide-react';
import { useEffect, useRef } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { formatTime } from '@/lib/format';
import { t } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { IconButton, Select } from '@/components/ui';
import { selectVisibleLogs, useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

const levelClass: Record<LogLevel, string> = {
  trace: 'text-muted-foreground/60',
  debug: 'text-muted-foreground',
  info: 'text-accent',
  warn: 'text-warning',
  error: 'text-danger',
};

const LEVELS: LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

export function LogPanel() {
  const open = useUiStore((s) => s.logOpen);
  const setLogOpen = useUiStore((s) => s.setLogOpen);
  const entries = useLogsStore(useShallow(selectVisibleLogs));
  const errorCount = useLogsStore((s) => s.errorCount);
  const errorFlag = useLogsStore((s) => s.errorFlag);
  const filterLevel = useLogsStore((s) => s.filterLevel);
  const setFilter = useLogsStore((s) => s.setFilter);
  const clear = useLogsStore((s) => s.clear);
  const ackError = useLogsStore((s) => s.ackError);
  const listRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    const el = listRef.current;
    if (!el || !open) return;
    if (stickToBottom.current) el.scrollTop = el.scrollHeight;
  }, [entries, open]);

  const onScroll = () => {
    const el = listRef.current;
    if (!el) return;
    stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
  };

  const toggle = () => {
    if (errorFlag) ackError();
    setLogOpen(!open);
  };

  return (
    <section className="flex h-full flex-col bg-background" aria-label={t('log.title')}>
      <header className="flex h-7 shrink-0 items-center gap-2 border-b border-border bg-muted pl-1 pr-1">
        <button
          type="button"
          onClick={toggle}
          aria-expanded={open}
          aria-controls="log-panel-body"
          className="flex h-6 items-center gap-1 rounded px-1 text-xs font-medium text-muted-foreground hover:bg-border/60 hover:text-foreground"
        >
          {open ? <ChevronDown size={13} /> : <ChevronUp size={13} />}
          {t('log.title')}
          <span className="text-[11px] font-normal">({entries.length})</span>
        </button>
        {errorCount > 0 && (
          <span className={cn('rounded-full px-1.5 text-[10px] font-semibold text-white', errorFlag ? 'bg-danger' : 'bg-danger/60')}>
            {errorCount} {t('log.errors')}
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          <div className="w-24">
            <Select<LogLevel> size="sm" aria-label={t('log.level')} value={filterLevel} onChange={setFilter} options={LEVELS.map((l) => ({ value: l, label: l }))} />
          </div>
          <IconButton label={t('log.clear')} size="sm" onClick={clear}>
            <Trash2 size={12} />
          </IconButton>
        </div>
      </header>
      {open && (
        <div id="log-panel-body" ref={listRef} onScroll={onScroll} className="min-h-0 flex-1 overflow-auto font-mono text-[12px] leading-5">
          {entries.length === 0 ? (
            <div className="p-2 text-muted-foreground">{t('log.empty')}</div>
          ) : (
            entries.map((e, i) => (
              <div key={`${e.at}-${i}`} className={cn('flex gap-2 whitespace-pre-wrap px-2 hover:bg-muted/60', e.level === 'error' && 'bg-danger/5')}>
                <span className="shrink-0 text-muted-foreground">{formatTime(e.at)}</span>
                <span className={cn('w-10 shrink-0 uppercase', levelClass[e.level])}>{e.level}</span>
                {e.jobId && <span className="shrink-0 text-muted-foreground">[{e.jobId}]</span>}
                {e.target && <span className="hidden shrink-0 text-muted-foreground/70 xl:inline">{e.target}</span>}
                <span className="min-w-0 flex-1 break-all">{e.message}</span>
              </div>
            ))
          )}
        </div>
      )}
    </section>
  );
}
