'use client';

import * as RadixSelect from '@radix-ui/react-select';
import { Check, ChevronsUpDown } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { hasWidthClass } from './Input';

export interface SelectOption<V extends string = string> {
  value: V;
  /** Plain text: typeahead, and what shows when `content` is not given. */
  label: string;
  /**
   * Rich row (e.g. a driver badge, the name and an EnvTag). Radix mirrors the chosen option's
   * content into the trigger, so the closed control shows it too.
   */
  content?: ReactNode;
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
  /** sm 22px, md 26px. */
  size?: 'sm' | 'md';
  /** As Radix: runs when the popup closes; `preventDefault()` keeps focus off the trigger. */
  onCloseAutoFocus?: (event: Event) => void;
}

export const selectContentClass =
  'animate-enter z-50 max-h-[min(360px,var(--radix-select-content-available-height))] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-[8px] bg-elevated p-1 text-base text-fg shadow-popover';
export const selectItemClass =
  'relative flex h-(--h-row-sm) select-none items-center rounded-[5px] pl-6 pr-2 outline-none data-[disabled]:opacity-45 data-[highlighted]:bg-selected-focus data-[highlighted]:text-on-accent';

/**
 * Radix select styled as a native popup button. Always controlled: `undefined` is passed to
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
  onCloseAutoFocus,
  ...aria
}: SelectProps<V>) {
  return (
    <RadixSelect.Root value={value ?? ''} onValueChange={(v) => onChange(v as V)} disabled={disabled}>
      <RadixSelect.Trigger
        id={id}
        aria-label={aria['aria-label']}
        className={cn(
          'input-focus inline-flex min-w-0 items-center justify-between gap-1 rounded-md border border-border-input bg-input px-2 text-left text-fg data-[placeholder]:text-fg-tertiary disabled:pointer-events-none disabled:opacity-50',
          !hasWidthClass(className) && 'w-full',
          size === 'sm' ? 'h-(--h-control-sm) text-sm' : 'h-(--h-control) text-base',
          className,
        )}
      >
        <span className="min-w-0 truncate">
          <RadixSelect.Value placeholder={placeholder} />
        </span>
        <RadixSelect.Icon>
          <ChevronsUpDown size={12} strokeWidth={1.75} className="shrink-0 text-fg-secondary" />
        </RadixSelect.Icon>
      </RadixSelect.Trigger>
      <RadixSelect.Portal>
        <RadixSelect.Content position="popper" sideOffset={4} className={selectContentClass} onCloseAutoFocus={onCloseAutoFocus}>
          <RadixSelect.Viewport>
            {options.map((o) => (
              <RadixSelect.Item key={o.value} value={o.value} disabled={o.disabled} textValue={o.label} className={selectItemClass}>
                <RadixSelect.ItemIndicator className="absolute left-1.5 inline-flex items-center">
                  <Check size={12} strokeWidth={2} />
                </RadixSelect.ItemIndicator>
                <RadixSelect.ItemText>{o.content ?? o.label}</RadixSelect.ItemText>
              </RadixSelect.Item>
            ))}
          </RadixSelect.Viewport>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
