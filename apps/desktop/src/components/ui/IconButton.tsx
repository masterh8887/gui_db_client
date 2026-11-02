'use client';

import { forwardRef, type ButtonHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';
import { Tooltip } from './Tooltip';

export interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** Accessible name; also shown as a tooltip. */
  label: string;
  /** sm 22px, md 26px. */
  size?: 'sm' | 'md';
  /** Pressed / on state: control fill and an accent icon. */
  active?: boolean;
  /** Set to hide the tooltip (e.g. inside menus). */
  noTooltip?: boolean;
}

/** Square ghost button with a 14px lucide icon (DESIGN.md §7.4). */
export const IconButton = forwardRef<HTMLButtonElement, IconButtonProps>(function IconButton(
  { label, size = 'md', active = false, noTooltip = false, className, children, type = 'button', ...rest },
  ref,
) {
  const btn = (
    <button
      ref={ref}
      type={type}
      aria-label={label}
      aria-pressed={active || undefined}
      className={cn(
        'inline-flex shrink-0 items-center justify-center rounded-md transition-colors duration-[80ms] hover:bg-hover active:bg-pressed disabled:pointer-events-none disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-focus',
        size === 'sm' ? 'h-(--h-control-sm) w-(--h-control-sm)' : 'h-(--h-control) w-(--h-control)',
        active ? 'bg-control text-accent' : 'text-fg-secondary hover:text-fg',
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
