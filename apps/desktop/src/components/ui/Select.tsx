'use client';

import * as RadixSelect from '@radix-ui/react-select';
import { Check, ChevronDown } from 'lucide-react';
import { cn } from '@/lib/utils';

export interface SelectOption<V extends string = string> {
  value: V;
  label: string;
  disabled?: boolean;
}

export interface SelectProps<V extends string = string> {
  value: V | undefined;
  onChange: (value: V) => void;
  options: SelectOption<V>[];
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  id?: string;
  'aria-label'?: string;
  size?: 'sm' | 'md';
}

/**
 * Radix select styled as a dense 28px control. Always controlled: `undefined` is passed to
 * Radix as `''`, which shows the placeholder (so the control never flips from uncontrolled to
 * controlled when a value arrives later).
 */
export function Select<V extends string = string>({
  value,
  onChange,
  options,
  placeholder,
  disabled,
  className,
  id,
  size = 'md',
  ...aria
}: SelectProps<V>) {
  return (
    <RadixSelect.Root value={value ?? ''} onValueChange={(v) => onChange(v as V)} disabled={disabled}>
      <RadixSelect.Trigger
        id={id}
        aria-label={aria['aria-label']}
        className={cn(
          'inline-flex w-full min-w-0 items-center justify-between gap-1 rounded border border-border bg-background px-2 text-left text-[13px] text-foreground data-[placeholder]:text-muted-foreground focus:border-accent focus:outline-none disabled:cursor-not-allowed disabled:opacity-50',
          size === 'sm' ? 'h-6 text-xs' : 'h-7',
          className,
        )}
      >
        <span className="truncate">
          <RadixSelect.Value placeholder={placeholder} />
        </span>
        <RadixSelect.Icon>
          <ChevronDown size={14} className="shrink-0 text-muted-foreground" />
        </RadixSelect.Icon>
      </RadixSelect.Trigger>
      <RadixSelect.Portal>
        <RadixSelect.Content
          position="popper"
          sideOffset={2}
          className="z-50 min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded border border-border bg-background shadow-lg"
        >
          <RadixSelect.Viewport className="p-1">
            {options.map((o) => (
              <RadixSelect.Item
                key={o.value}
                value={o.value}
                disabled={o.disabled}
                className="relative flex h-7 cursor-default select-none items-center rounded pl-6 pr-2 text-[13px] outline-none data-[disabled]:opacity-50 data-[highlighted]:bg-accent data-[highlighted]:text-accent-foreground"
              >
                <RadixSelect.ItemIndicator className="absolute left-1.5 inline-flex items-center">
                  <Check size={12} />
                </RadixSelect.ItemIndicator>
                <RadixSelect.ItemText>{o.label}</RadixSelect.ItemText>
              </RadixSelect.Item>
            ))}
          </RadixSelect.Viewport>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
