'use client';

import * as RadixTabs from '@radix-ui/react-tabs';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface TabItem<V extends string = string> {
  value: V;
  label: ReactNode;
  disabled?: boolean;
}

export interface TabsProps<V extends string = string> {
  value: V;
  onChange: (value: V) => void;
  items: TabItem<V>[];
  /** Content per tab value. Unmounted when inactive. */
  children: (value: V) => ReactNode;
  className?: string;
  /** Extra content on the right side of the tab strip. */
  trailing?: ReactNode;
}

/** Radix tabs with a 28px strip; the active panel fills the remaining height. */
export function Tabs<V extends string = string>({ value, onChange, items, children, className, trailing }: TabsProps<V>) {
  return (
    <RadixTabs.Root value={value} onValueChange={(v) => onChange(v as V)} className={cn('flex h-full min-h-0 flex-col', className)}>
      <div className="flex h-7 shrink-0 items-stretch border-b border-border bg-muted">
        <RadixTabs.List className="flex items-stretch">
          {items.map((it) => (
            <RadixTabs.Trigger
              key={it.value}
              value={it.value}
              disabled={it.disabled}
              className="relative px-3 text-[13px] text-muted-foreground outline-none hover:text-foreground data-[state=active]:bg-background data-[state=active]:text-foreground data-[state=active]:after:absolute data-[state=active]:after:inset-x-0 data-[state=active]:after:top-0 data-[state=active]:after:h-0.5 data-[state=active]:after:bg-accent disabled:cursor-not-allowed disabled:opacity-40"
            >
              {it.label}
            </RadixTabs.Trigger>
          ))}
        </RadixTabs.List>
        {trailing && <div className="ml-auto flex items-center gap-1 px-1">{trailing}</div>}
      </div>
      {items.map((it) => (
        <RadixTabs.Content key={it.value} value={it.value} className="min-h-0 flex-1 overflow-hidden outline-none">
          {value === it.value ? children(it.value) : null}
        </RadixTabs.Content>
      ))}
    </RadixTabs.Root>
  );
}
