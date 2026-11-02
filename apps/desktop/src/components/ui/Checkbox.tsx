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
        'inline-flex h-[14px] w-[14px] shrink-0 items-center justify-center rounded-[4px] bg-btn text-on-accent shadow-btn data-[state=checked]:bg-accent-fill data-[state=checked]:shadow-none focus-visible:outline-2 focus-visible:outline-focus disabled:pointer-events-none disabled:opacity-45',
        className,
      )}
    >
      <RadixCheckbox.Indicator>
        <Check size={10} strokeWidth={3} />
      </RadixCheckbox.Indicator>
    </RadixCheckbox.Root>
  );
}
