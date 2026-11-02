'use client';

// App-wide hosts mounted once by the AppShell: the confirm sheet and the toast stack.

import { CircleCheck, CircleX, Info, X } from 'lucide-react';
import { useEffect } from 'react';
import { answerConfirm, registerConfirmHost, useConfirmQueue } from '@/lib/confirm';
import { dismissToast, useToastStore } from '@/lib/toast';
import { cn } from '@/lib/utils';
import { Button } from './Button';
import { Sheet } from './Sheet';
import { us } from './strings';

/** Shows the oldest `askConfirm` request as a small sheet (Esc / scrim / Cancel = false). */
export function ConfirmHost() {
  const req = useConfirmQueue((s) => s.queue[0]);
  useEffect(() => registerConfirmHost(), []);
  if (!req) return null;
  const { options } = req;
  return (
    <Sheet
      key={req.id}
      open
      onOpenChange={(o) => {
        if (!o) answerConfirm(req.id, false);
      }}
      title={options.title}
      width="sm"
      footer={
        <>
          <Button variant="secondary" onClick={() => answerConfirm(req.id, false)}>
            {options.cancelLabel ?? us('cancel')}
          </Button>
          <Button variant={options.danger ? 'dangerFilled' : 'primary'} autoFocus onClick={() => answerConfirm(req.id, true)}>
            {options.confirmLabel ?? us('confirm')}
          </Button>
        </>
      }
    >
      {options.body ? <p className="text-base text-fg-secondary">{options.body}</p> : null}
    </Sheet>
  );
}

const TOAST_ICON = { neutral: Info, success: CircleCheck, danger: CircleX } as const;
const TOAST_ICON_CLASS = { neutral: 'text-fg-secondary', success: 'text-success', danger: 'text-danger' } as const;

/** Toast stack, bottom centre; background outcomes only (DESIGN.md §3.3). */
export function Toaster() {
  const toasts = useToastStore((s) => s.toasts);
  return (
    <div aria-live="polite" className="pointer-events-none fixed inset-x-0 bottom-6 z-[70] flex flex-col items-center gap-2 px-4">
      {toasts.map((t) => {
        const Icon = TOAST_ICON[t.tone];
        return (
          <div
            key={t.id}
            role={t.tone === 'danger' ? 'alert' : 'status'}
            className="animate-enter-up pointer-events-auto flex max-w-[560px] items-center gap-2 rounded-[8px] bg-elevated py-1.5 pr-1.5 pl-3 text-base text-fg shadow-popover"
          >
            <Icon size={14} strokeWidth={1.75} className={cn('shrink-0', TOAST_ICON_CLASS[t.tone])} />
            <span className="min-w-0 flex-1" data-selectable>
              {t.message}
            </span>
            <button
              type="button"
              aria-label={us('dismiss')}
              onClick={() => dismissToast(t.id)}
              className="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded-[4px] text-fg-tertiary hover:bg-hover hover:text-fg"
            >
              <X size={12} />
            </button>
          </div>
        );
      })}
    </div>
  );
}
