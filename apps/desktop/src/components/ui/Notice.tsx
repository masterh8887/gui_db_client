'use client';

import { CircleCheck, CircleX, Info, TriangleAlert, X } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { IconButton } from './IconButton';
import { us } from './strings';

export type NoticeTone = 'info' | 'success' | 'warning' | 'danger';

const tones: Record<NoticeTone, { cls: string; icon: string; Icon: typeof Info }> = {
  info: { cls: 'bg-accent-soft', icon: 'text-accent', Icon: Info },
  success: { cls: 'bg-success-soft', icon: 'text-success', Icon: CircleCheck },
  warning: { cls: 'bg-warning-soft', icon: 'text-warning', Icon: TriangleAlert },
  danger: { cls: 'bg-danger-soft', icon: 'text-danger', Icon: CircleX },
};

/** Inline tinted message (no border, radius 6). Lives where the action happened. */
export function Notice({
  tone = 'info',
  children,
  onClose,
  className,
  title,
}: {
  tone?: NoticeTone;
  children: ReactNode;
  onClose?: () => void;
  className?: string;
  title?: ReactNode;
}) {
  const { cls, icon, Icon } = tones[tone];
  return (
    <div
      role={tone === 'danger' || tone === 'warning' ? 'alert' : 'status'}
      className={cn('flex items-start gap-2 rounded-md px-2.5 py-1.5 text-base text-fg', cls, className)}
    >
      <Icon size={14} strokeWidth={1.75} className={cn('mt-0.5 shrink-0', icon)} />
      <div className="min-w-0 flex-1 break-words" data-selectable>
        {title && <div className="font-medium">{title}</div>}
        {children}
      </div>
      {onClose && (
        <IconButton label={us('close')} size="sm" noTooltip onClick={onClose} className="-mr-1 -mt-0.5">
          <X size={12} />
        </IconButton>
      )}
    </div>
  );
}
