'use client';

import { cva, type VariantProps } from 'class-variance-authority';
import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { Spinner } from './Spinner';

// DESIGN.md §7.4: primary (accent fill, one per region), secondary (raised white), ghost,
// danger (red text; `dangerFilled` only in confirms), link. sm 22px / md 26px, radius 6.
// `outline` is an alias of `secondary`; `lg` is kept for older callers (28px). Filled variants
// use the `*-fill` tokens (white text >= 4.5:1 in both themes); `--accent-hover` and `--danger`
// are text colours only.
export const buttonVariants = cva(
  'inline-flex shrink-0 select-none items-center justify-center gap-1.5 whitespace-nowrap rounded-md font-normal transition-colors duration-[80ms] disabled:pointer-events-none disabled:opacity-45 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-focus',
  {
    variants: {
      variant: {
        primary: 'bg-accent-fill text-on-accent hover:bg-accent-fill-hover active:brightness-95',
        secondary: 'bg-btn text-fg shadow-btn hover:bg-btn-hover active:bg-btn-pressed',
        outline: 'bg-btn text-fg shadow-btn hover:bg-btn-hover active:bg-btn-pressed',
        ghost: 'bg-transparent text-fg hover:bg-hover active:bg-pressed',
        danger: 'bg-transparent text-danger hover:bg-danger-soft active:bg-pressed',
        dangerFilled: 'bg-danger-fill text-on-accent hover:bg-danger-fill-hover active:brightness-95',
        link: 'bg-transparent text-accent underline-offset-2 hover:underline',
      },
      size: {
        sm: 'h-(--h-control-sm) px-2 text-sm',
        md: 'h-(--h-control) px-2.5 text-base',
        lg: 'h-7 px-3 text-base',
      },
    },
    // A link is inline text: no control height or padding whatever the size (after the size classes,
    // so `cn` keeps these).
    compoundVariants: [{ variant: 'link', class: 'h-auto px-0' }],
    defaultVariants: { variant: 'secondary', size: 'md' },
  },
);

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement>, VariantProps<typeof buttonVariants> {
  loading?: boolean;
  icon?: ReactNode;
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { className, variant, size, loading = false, icon, children, disabled, type = 'button', ...rest },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      className={cn(buttonVariants({ variant, size }), className)}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      {...rest}
    >
      {loading ? <Spinner size={12} tone="current" decorative /> : icon}
      {children}
    </button>
  );
});
