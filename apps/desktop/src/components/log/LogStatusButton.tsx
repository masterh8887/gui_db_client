'use client';

// Console toggle for the toolbar and the Welcome header (DESIGN.md §5.G): a 28px icon button
// with an error badge (solid while the error flag is up, 50% once acknowledged). Click toggles
// the console and acknowledges the errors.

import { SquareTerminal } from 'lucide-react';
import { comboLabel } from '@/lib/keymap';
import { useIsMac } from '@/lib/platform';
import { cn } from '@/lib/utils';
import { Tooltip } from '@/components/ui';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { cs } from './strings';

export function LogStatusButton() {
  const open = useUiStore((s) => s.logOpen);
  const errorFlag = useLogsStore((s) => s.errorFlag);
  const errorCount = useLogsStore((s) => s.errorCount);
  const mac = useIsMac();
  const label = errorCount > 0 ? cs('buttonErrors', { n: errorCount }) : cs('button');

  return (
    <Tooltip content={`${cs('button')} (${comboLabel('mod+j', mac)})`} delay={300}>
      <button
        type="button"
        aria-label={label}
        aria-pressed={open}
        data-error-flag={errorFlag || undefined}
        onClick={() => {
          useLogsStore.getState().ackError();
          useUiStore.getState().toggleLog();
        }}
        className={cn(
          'relative inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md transition-colors duration-[80ms] hover:bg-hover active:bg-pressed focus-visible:outline-2 focus-visible:outline-focus',
          open ? 'bg-control text-accent' : 'text-fg-secondary hover:text-fg',
        )}
      >
        <SquareTerminal size={16} strokeWidth={1.75} />
        {errorCount > 0 && (
          <span
            aria-hidden
            data-testid="console-error-badge"
            className={cn(
              'absolute -top-0.5 -right-0.5 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-danger px-1 text-xs leading-none font-semibold tabular-nums text-on-accent',
              !errorFlag && 'opacity-50',
            )}
          >
            {errorCount > 99 ? '99+' : errorCount}
          </span>
        )}
      </button>
    </Tooltip>
  );
}
