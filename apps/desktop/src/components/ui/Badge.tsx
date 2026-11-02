import { cva, type VariantProps } from 'class-variance-authority';
import type { HTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

/** Soft-tinted chip, no border, 18px; 11px Latin, 12px when the text holds Hangul (pass `hangul`). */
export const badgeVariants = cva(
  'inline-flex h-[18px] shrink-0 items-center gap-1 whitespace-nowrap rounded-sm px-1.5 font-medium leading-none tabular-nums',
  {
    variants: {
      tone: {
        neutral: 'bg-control text-fg-secondary',
        accent: 'bg-accent-soft text-accent',
        success: 'bg-success-soft text-success',
        warning: 'bg-warning-soft text-warning',
        danger: 'bg-danger-soft text-danger',
        outline: 'bg-transparent text-fg-secondary shadow-[inset_0_0_0_1px_var(--separator-strong)]',
      },
      hangul: {
        true: 'text-sm',
        false: 'text-xs',
      },
    },
    defaultVariants: { tone: 'neutral', hangul: false },
  },
);

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement>, VariantProps<typeof badgeVariants> {}

const HANGUL = /[ㄱ-힝]/;

export function Badge({ className, tone, hangul, children, ...rest }: BadgeProps) {
  const isHangul = hangul ?? (typeof children === 'string' && HANGUL.test(children));
  return (
    <span className={cn(badgeVariants({ tone, hangul: isHangul }), className)} {...rest}>
      {children}
    </span>
  );
}
