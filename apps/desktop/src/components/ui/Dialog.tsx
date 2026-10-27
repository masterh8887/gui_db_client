'use client';

import * as RadixDialog from '@radix-ui/react-dialog';
import { X } from 'lucide-react';
import type { ReactNode } from 'react';
import { t } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { IconButton } from './IconButton';

export interface DialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  children: ReactNode;
  /** Buttons row. */
  footer?: ReactNode;
  width?: 'sm' | 'md' | 'lg' | 'xl';
  /** Block closing by Esc / overlay click (e.g. while a request is in flight). */
  modal?: boolean;
  className?: string;
}

const widths = { sm: 'w-[380px]', md: 'w-[520px]', lg: 'w-[680px]', xl: 'w-[860px]' };

export function Dialog({ open, onOpenChange, title, description, children, footer, width = 'md', modal = false, className }: DialogProps) {
  return (
    <RadixDialog.Root open={open} onOpenChange={onOpenChange}>
      <RadixDialog.Portal>
        <RadixDialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <RadixDialog.Content
          onEscapeKeyDown={modal ? (e) => e.preventDefault() : undefined}
          onPointerDownOutside={modal ? (e) => e.preventDefault() : undefined}
          onInteractOutside={modal ? (e) => e.preventDefault() : undefined}
          className={cn(
            'fixed left-1/2 top-1/2 z-50 flex max-h-[90vh] max-w-[95vw] -translate-x-1/2 -translate-y-1/2 flex-col rounded-md border border-border bg-background shadow-xl focus:outline-none',
            widths[width],
            className,
          )}
        >
          <div className="flex h-9 shrink-0 items-center justify-between border-b border-border pl-3 pr-1">
            <RadixDialog.Title className="text-[13px] font-semibold">{title}</RadixDialog.Title>
            <RadixDialog.Close asChild>
              <IconButton label={t('common.close')} size="sm" noTooltip>
                <X size={14} />
              </IconButton>
            </RadixDialog.Close>
          </div>
          {description ? (
            <RadixDialog.Description className="px-3 pt-2 text-xs text-muted-foreground">{description}</RadixDialog.Description>
          ) : (
            <RadixDialog.Description className="sr-only">{typeof title === 'string' ? title : ''}</RadixDialog.Description>
          )}
          <div className="min-h-0 flex-1 overflow-auto p-3">{children}</div>
          {footer && <div className="flex shrink-0 items-center justify-end gap-2 border-t border-border px-3 py-2">{footer}</div>}
        </RadixDialog.Content>
      </RadixDialog.Portal>
    </RadixDialog.Root>
  );
}
