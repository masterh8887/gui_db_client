'use client';

import * as RadixDialog from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { escapeIsLocal } from './escape';
import { IconButton } from './IconButton';
import { us } from './strings';

// Dialog and Sheet share one chrome (DESIGN.md §5.I): `bg-elevated`, radius 10, `shadow-dialog`,
// `--scrim`, 44px header (title 15/600 + optional 12px description), body 16px 20px, footer with
// a hairline and right-aligned buttons (optional left slot), no corner X by default.
// Esc, scrim click and Cancel all dismiss through `onOpenChange(false)`.

export type ModalWidth = 'sm' | 'md' | 'lg' | 'xl';
export const MODAL_WIDTHS: Record<ModalWidth, string> = {
  sm: 'w-[420px]',
  md: 'w-[560px]',
  lg: 'w-[720px]',
  xl: 'w-[880px]',
};

export interface ModalChromeProps {
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  footerStart?: ReactNode;
  showClose?: boolean;
  /** Body padding off (lists that run edge to edge). */
  flush?: boolean;
}

/** Header / body / footer of a Dialog or Sheet (must render inside `RadixDialog.Content`). */
export function ModalChrome({ title, description, children, footer, footerStart, showClose = false, flush = false }: ModalChromeProps) {
  return (
    <>
      <div className="flex min-h-11 shrink-0 items-center gap-2 px-5 pt-3 pb-1">
        <div className="min-w-0 flex-1">
          <RadixDialog.Title className="truncate text-lg font-semibold">{title}</RadixDialog.Title>
          {description ? (
            <RadixDialog.Description className="mt-0.5 text-sm text-fg-secondary">{description}</RadixDialog.Description>
          ) : (
            <RadixDialog.Description className="sr-only">{typeof title === 'string' ? title : ''}</RadixDialog.Description>
          )}
        </div>
        {showClose && (
          <RadixDialog.Close asChild>
            <IconButton label={us('close')} size="sm" noTooltip className="-mr-2">
              <X size={14} />
            </IconButton>
          </RadixDialog.Close>
        )}
      </div>
      <div className={cn('min-h-0 flex-1 overflow-auto', flush ? '' : 'px-5 pt-2 pb-4')}>{children}</div>
      {(footer || footerStart) && (
        <div className="flex shrink-0 items-center gap-2 border-t border-separator px-5 py-3">
          {footerStart && <div className="mr-auto flex min-w-0 items-center gap-2">{footerStart}</div>}
          <div className="ml-auto flex items-center gap-2">{footer}</div>
        </div>
      )}
    </>
  );
}

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  /** Buttons row (right-aligned; primary rightmost). */
  footer?: ReactNode;
  /** Left slot of the footer. */
  footerStart?: ReactNode;
  width?: ModalWidth;
  /** Block closing by Esc / scrim click (e.g. while a request is in flight). */
  modal?: boolean;
  /** Corner close button (default off). */
  showClose?: boolean;
  className?: string;
}

/** Centred dialog, 15vh from the top. */
export function Dialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  footerStart,
  width = 'md',
  modal = false,
  showClose = false,
  className,
}: DialogProps) {
  return (
    <RadixDialog.Root open={open} onOpenChange={onOpenChange}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="animate-fade fixed inset-0 z-40 bg-[var(--scrim)]" />
        <RadixDialog.Content
          onEscapeKeyDown={(e) => {
            if (modal || escapeIsLocal(e)) e.preventDefault();
          }}
          onPointerDownOutside={modal ? (e) => e.preventDefault() : undefined}
          onInteractOutside={modal ? (e) => e.preventDefault() : undefined}
          className={cn(
            'animate-enter fixed left-1/2 top-[15vh] z-50 flex max-h-[80vh] max-w-[calc(100vw-32px)] -translate-x-1/2 flex-col rounded-lg bg-elevated text-fg shadow-dialog outline-none',
            MODAL_WIDTHS[width],
            className,
          )}
        >
          <ModalChrome title={title} description={description} footer={footer} footerStart={footerStart} showClose={showClose}>
            {children}
          </ModalChrome>
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}
