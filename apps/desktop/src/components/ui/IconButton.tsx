'use client';

import { forwardRef, type ButtonHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';
import { Tooltip } from './Tooltip';

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** Accessible name; also shown as a tooltip. */
  label: string;
  size?: 'sm' | 'md';
  active?: boolean;
  /** Set to hide the tooltip (e.g. inside menus). */
  noTooltip?: boolean;
}

export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  { label, size = 'md', active = false, noTooltip = false, className, children, type = 'button', ...rest },
  ref,
) {
  const btn = (
    <button
      ref={ref}
      type={type}
      aria-label={label}
      className={cn(
        'inline-flex shrink-0 items-center justify-center rounded border border-transparent transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-accent',
        size === 'sm' ? 'h-5 w-5' : 'h-7 w-7',
        active ? 'bg-muted text-foreground' : 'text-muted-foreground',
        className,
      )}
      {...rest}
    >
      {children}
    </button>
  );
  if (noTooltip) return btn;
  return <Tooltip content={label}>{btn}</Tooltip>;
});
