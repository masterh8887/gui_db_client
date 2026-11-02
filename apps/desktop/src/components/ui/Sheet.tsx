'use client';

import * as RadixDialog from '@radix-ui/react-dialog';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { ModalChrome, MODAL_WIDTHS, type ModalWidth } from './Dialog';
import { escapeIsLocal } from './escape';

export interface SheetProps {
  open: boolean;
  /** Esc / scrim → `onOpenChange(false)`; callers map it to the cancel answer. */
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  footerStart?: ReactNode;
  width?: ModalWidth;
  /** `false` blocks Esc and scrim (e.g. while starting). Default true. */
  dismissible?: boolean;
  /** Body without padding. */
  flush?: boolean;
  className?: string;
}

/**
 * Sheet: a dialog that drops from under the toolbar (`top: var(--sheet-top)`, 48px in a
 * workspace, 16px on Welcome). Same chrome as `Dialog` (DESIGN.md §5.I).
 */
export function Sheet({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  footerStart,
  width = 'md',
  dismissible = true,
  flush = false,
  className,
}: SheetProps) {
  const block = (e: Event) => {
    if (!dismissible) e.preventDefault();
  };
  return (
    <RadixDialog.Root open={open} onOpenChange={(o) => (o || dismissible ? onOpenChange(o) : undefined)}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="animate-fade fixed inset-0 z-40 bg-[var(--scrim)]" />
        <RadixDialog.Content
          onEscapeKeyDown={(e) => {
            if (!dismissible || escapeIsLocal(e)) e.preventDefault();
          }}
          onPointerDownOutside={block}
          onInteractOutside={block}
          className={cn(
            'animate-enter fixed left-1/2 top-(--sheet-top) z-50 flex max-h-[calc(100vh-var(--sheet-top)-24px)] max-w-[calc(100vw-32px)] -translate-x-1/2 flex-col rounded-lg bg-elevated text-fg shadow-dialog outline-none',
            MODAL_WIDTHS[width],
            className,
          )}
        >
          <ModalChrome title={title} description={description} footer={footer} footerStart={footerStart} flush={flush}>
            {children}
          </ModalChrome>
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}
