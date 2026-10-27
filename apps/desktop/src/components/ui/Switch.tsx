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
        'relative inline-flex h-4 w-7 shrink-0 cursor-pointer items-center rounded-full border border-border bg-muted transition-colors data-[state=checked]:border-accent data-[state=checked]:bg-accent focus-visible:outline-2 focus-visible:outline-accent disabled:cursor-not-allowed disabled:opacity-50',
        className,
      )}
    >
      <RadixSwitch.Thumb className="block h-3 w-3 translate-x-0.5 rounded-full bg-background shadow transition-transform data-[state=checked]:translate-x-3.5" />
    </RadixSwitch.Root>
  );
}
