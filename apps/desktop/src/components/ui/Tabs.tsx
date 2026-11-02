'use client';

import * as RadixTabs from '@radix-ui/react-tabs';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface TabsItem<V extends string = string> {
  value: V;
  label: ReactNode;
  disabled?: boolean;
}

export interface TabsProps<V extends string = string> {
  value: V;
  onChange: (value: V) => void;
  items: TabsItem<V>[];
  /** Content per tab value. Unmounted when inactive. */
  children: (value: V) => ReactNode;
  className?: string;
  /** Extra content on the right side of the strip. */
  trailing?: ReactNode;
}

/**
 * Radix tabs with panels, styled like the track `SegmentedControl` (grey track, white thumb).
 * For a bare switcher without panels use `SegmentedControl`.
 */
export function Tabs<V extends string = string>({ value, onChange, items, children, className, trailing }: TabsProps<V>) {
  return (
    <RadixTabs.Root value={value} onValueChange={(v) => onChange(v as V)} className={cn('flex h-full min-h-0 flex-col', className)}>
      <div className="flex h-(--h-bar) shrink-0 items-center gap-2 border-b border-separator px-2">
        <RadixTabs.List className="inline-flex h-[22px] items-stretch gap-0.5 rounded-md bg-control p-0.5">
          {items.map((it) => (
            <RadixTabs.Trigger
              key={it.value}
              value={it.value}
              disabled={it.disabled}
              className="rounded-[5px] px-2.5 text-sm text-fg-secondary outline-none focus-visible:outline-2 focus-visible:outline-focus data-[state=active]:bg-seg-thumb data-[state=active]:font-medium data-[state=active]:text-fg data-[state=active]:shadow-seg disabled:pointer-events-none disabled:opacity-40"
            >
              {it.label}
            </RadixTabs.Trigger>
          ))}
        </RadixTabs.List>
        {trailing && <div className="ml-auto flex items-center gap-1">{trailing}</div>}
      </div>
      {items.map((it) => (
        <RadixTabs.Content key={it.value} value={it.value} className="min-h-0 flex-1 overflow-hidden outline-none">
          {value === it.value ? children(it.value) : null}
        </RadixTabs.Content>
      ))}
    </RadixTabs.Root>
  );
}
