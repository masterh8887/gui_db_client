'use client';

import * as RadixSwitch from '@radix-ui/react-switch';
import { cn } from '@/lib/utils';

export interface SwitchProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
  id?: string;
  className?: string;
  'aria-label'?: string;
}

export function Switch({ checked, onChange, disabled, id, className, ...aria }: SwitchProps) {
  return (
    <RadixSwitch.Root
      id={id}
      checked={checked}
      onCheckedChange={onChange}
      disabled={disabled}
      aria-label={aria['aria-label']}
      className={cn(
        'relative inline-flex h-4 w-[26px] shrink-0 items-center rounded-full bg-control shadow-[inset_0_0_0_0.5px_var(--separator-strong)] transition-colors duration-[80ms] data-[state=checked]:bg-accent-fill focus-visible:outline-2 focus-visible:outline-focus disabled:pointer-events-none disabled:opacity-45',
        className,
      )}
    >
      <RadixSwitch.Thumb className="block h-3 w-3 translate-x-0.5 rounded-full bg-white shadow-seg transition-transform duration-[80ms] data-[state=checked]:translate-x-[12px]" />
    </RadixSwitch.Root>
  );
}
