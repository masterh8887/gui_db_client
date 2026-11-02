'use client';

import { useRef, type KeyboardEvent, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SegmentOption<V extends string> {
  value: V;
  label: ReactNode;
  icon?: ReactNode;
  badge?: ReactNode;
  disabled?: boolean;
  /** Tooltip (also the reason of a disabled option). */
  title?: string;
  /** Hairline before this option (groups in the query bar). */
  separatorBefore?: boolean;
}

export interface SegmentedControlProps<V extends string> {
  value: V;
  onChange: (v: V) => void;
  /** Fires on every click / Enter / Space, the active option included (toggle a panel). */
  onActivate?: (v: V) => void;
  /** `track` (default): grey track + white thumb, role=radiogroup. `tabs`: underline, role=tablist. */
  variant?: 'track' | 'tabs';
  /** sm: 22px track; md: 26px. */
  size?: 'sm' | 'md';
  'aria-label': string;
  className?: string;
  /** Segments share the full width (sidebar 항목 | 기록). */
  fill?: boolean;
  options: SegmentOption<V>[];
}

/**
 * Segmented control (DESIGN.md §7.4). Arrow keys (and Home / End) move the selection and focus
 * (roving tabindex); Enter / Space / click select and fire `onActivate`. Never an accent fill.
 */
export function SegmentedControl<V extends string>({
  value,
  onChange,
  onActivate,
  variant = 'track',
  size = 'sm',
  className,
  fill = false,
  options,
  ...aria
}: SegmentedControlProps<V>) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const tabs = variant === 'tabs';
  const enabled = options.map((o, i) => (o.disabled ? -1 : i)).filter((i) => i >= 0);
  const activeIndex = options.findIndex((o) => o.value === value);
  // The focusable option: the active one, else the first enabled one.
  const focusIndex = activeIndex >= 0 && !options[activeIndex].disabled ? activeIndex : (enabled[0] ?? -1);

  const select = (i: number, activate: boolean) => {
    const o = options[i];
    if (!o || o.disabled) return;
    if (o.value !== value) onChange(o.value);
    if (activate) onActivate?.(o.value);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLButtonElement>, i: number) => {
    const pos = enabled.indexOf(i);
    let next: number | undefined;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = enabled[(pos + 1) % enabled.length];
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = enabled[(pos - 1 + enabled.length) % enabled.length];
    else if (e.key === 'Home') next = enabled[0];
    else if (e.key === 'End') next = enabled[enabled.length - 1];
    else if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      select(i, true);
      return;
    }
    if (next === undefined) return;
    e.preventDefault();
    refs.current[next]?.focus();
    select(next, false);
  };

  return (
    <div
      role={tabs ? 'tablist' : 'radiogroup'}
      aria-label={aria['aria-label']}
      className={cn(
        'inline-flex shrink-0 items-stretch',
        fill && 'w-full',
        tabs ? 'h-full gap-3' : cn('gap-0.5 rounded-md bg-control p-0.5', size === 'sm' ? 'h-[22px]' : 'h-(--h-control)'),
        className,
      )}
    >
      {options.map((o, i) => {
        const active = o.value === value;
        return (
          <div key={o.value} className={cn('flex items-stretch', fill && 'flex-1')}>
            {o.separatorBefore && <span aria-hidden className={cn('self-center bg-separator-strong', tabs ? 'mr-3 h-3.5 w-px' : 'mx-0.5 h-3 w-px')} />}
            <button
              ref={(el) => {
                refs.current[i] = el;
              }}
              type="button"
              role={tabs ? 'tab' : 'radio'}
              aria-checked={tabs ? undefined : active}
              aria-selected={tabs ? active : undefined}
              aria-disabled={o.disabled || undefined}
              data-roving={tabs ? '' : undefined}
              data-state={active ? 'active' : 'inactive'}
              tabIndex={i === focusIndex ? 0 : -1}
              title={o.title}
              onClick={() => select(i, true)}
              onKeyDown={(e) => onKeyDown(e, i)}
              className={cn(
                'relative inline-flex items-center justify-center gap-1 whitespace-nowrap outline-none focus-visible:outline-2 focus-visible:outline-focus',
                fill && 'flex-1',
                o.disabled && 'opacity-40',
                tabs
                  ? cn(
                      'px-0.5 text-sm',
                      active
                        ? 'font-medium text-fg after:absolute after:inset-x-0 after:bottom-0 after:h-0.5 after:rounded-full after:bg-fg'
                        : 'text-fg-secondary hover:text-fg',
                    )
                  : cn(
                      'rounded-[5px] px-2.5',
                      size === 'sm' ? 'text-sm' : 'text-base',
                      active ? 'bg-seg-thumb font-medium text-fg shadow-seg' : 'text-fg-secondary hover:text-fg',
                    ),
              )}
            >
              {o.icon && <span className="inline-flex [&_svg]:size-3.5">{o.icon}</span>}
              {o.label}
              {o.badge !== undefined && o.badge !== null && o.badge !== false && <span className="inline-flex items-center">{o.badge}</span>}
            </button>
          </div>
        );
      })}
    </div>
  );
}
