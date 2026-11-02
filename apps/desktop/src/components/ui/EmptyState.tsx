import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

/** Quiet empty / placeholder message: 13px secondary title, 12px hint (DESIGN.md §7.4). */
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
        'flex flex-col items-center justify-center gap-1.5 text-center text-fg-secondary',
        compact ? 'px-3 py-6' : 'h-full p-6',
        className,
      )}
    >
      {icon && <div className="mb-1 text-fg-tertiary">{icon}</div>}
      <div className="text-base">{title}</div>
      {hint && <div className="text-sm text-fg-secondary">{hint}</div>}
      {action && <div className="mt-2 flex items-center gap-2">{action}</div>}
    </div>
  );
}
