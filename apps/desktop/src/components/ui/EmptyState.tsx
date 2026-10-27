import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export function EmptyState({
  icon,
  title,
  hint,
  action,
  className,
  compact = false,
}: {
  icon?: ReactNode;
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
  className?: string;
  /** Natural height with a little padding, instead of filling and centring in the parent. */
  compact?: boolean;
}) {
  return (
    <div
      className={cn(
        'flex flex-col items-center justify-center gap-2 text-center text-muted-foreground',
        compact ? 'px-2 py-6' : 'h-full p-6',
        className,
      )}
    >
      {icon && <div className="text-muted-foreground/60">{icon}</div>}
      <div className="text-[13px]">{title}</div>
      {hint && <div className="text-xs">{hint}</div>}
      {action && <div className="mt-1">{action}</div>}
    </div>
  );
}
