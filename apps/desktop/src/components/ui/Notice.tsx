'use client';

import { AlertTriangle, CheckCircle2, Info, X, XCircle } from 'lucide-react';
import type { ReactNode } from 'react';
import { t } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { IconButton } from './IconButton';

export type NoticeTone = 'info' | 'success' | 'warning' | 'danger';

const tones: Record<NoticeTone, { cls: string; Icon: typeof Info }> = {
  info: { cls: 'border-accent/40 bg-accent/10 text-foreground', Icon: Info },
  success: { cls: 'border-success/40 bg-success/10 text-foreground', Icon: CheckCircle2 },
  warning: { cls: 'border-warning/50 bg-warning/10 text-foreground', Icon: AlertTriangle },
  danger: { cls: 'border-danger/50 bg-danger/10 text-foreground', Icon: XCircle },
};

/** Inline toast-like message (no global toast stack: notices live where the action happened). */
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
  const { cls, Icon } = tones[tone];
  return (
    <div role={tone === 'danger' || tone === 'warning' ? 'alert' : 'status'} className={cn('flex items-start gap-2 rounded border px-2 py-1.5 text-[13px]', cls, className)}>
      <Icon size={14} className={cn('mt-0.5 shrink-0', tone === 'danger' ? 'text-danger' : tone === 'warning' ? 'text-warning' : tone === 'success' ? 'text-success' : 'text-accent')} />
      <div className="min-w-0 flex-1 break-words">
        {title && <div className="font-medium">{title}</div>}
        {children}
      </div>
      {onClose && (
        <IconButton label={t('common.close')} size="sm" noTooltip onClick={onClose} className="-mr-1 -mt-0.5">
          <X size={12} />
        </IconButton>
      )}
    </div>
  );
}
