'use client';

import * as RadixCheckbox from '@radix-ui/react-checkbox';
import { Check } from 'lucide-react';
import { cn } from '@/lib/utils';

export interface CheckboxProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
  id?: string;
  className?: string;
  'aria-label'?: string;
}

export function Checkbox({ checked, onChange, disabled, id, className, ...aria }: CheckboxProps) {
  return (
    <RadixCheckbox.Root
      id={id}
      checked={checked}
      onCheckedChange={(v) => onChange(v === true)}
      disabled={disabled}
      aria-label={aria['aria-label']}
      className={cn(
        'inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-sm border border-border bg-background data-[state=checked]:border-accent data-[state=checked]:bg-accent data-[state=checked]:text-accent-foreground focus-visible:outline-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
    >
      <RadixCheckbox.Indicator>
        <Check size={12} strokeWidth={3} />
      </RadixCheckbox.Indicator>
    </RadixCheckbox.Root>
  );
}
