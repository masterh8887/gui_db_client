'use client';

import { cva, type VariantProps } from 'class-variance-authority';
import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { Spinner } from './Spinner';

export const buttonVariants = cva(
  'inline-flex select-none items-center justify-center gap-1.5 whitespace-nowrap rounded border text-[13px] font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent',
  {
    variants: {
      variant: {
        primary: 'border-accent bg-accent text-accent-foreground hover:brightness-110',
        secondary: 'border-border bg-muted text-foreground hover:bg-border/60',
        outline: 'border-border bg-transparent text-foreground hover:bg-muted',
        ghost: 'border-transparent bg-transparent text-foreground hover:bg-muted',
        danger: 'border-danger bg-danger text-white hover:brightness-110',
        link: 'border-transparent bg-transparent text-accent underline-offset-2 hover:underline',
      },
      size: {
        sm: 'h-6 px-2 text-xs',
        md: 'h-7 px-2.5',
        lg: 'h-8 px-3.5',
      },
    },
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
      {loading ? <Spinner size={12} /> : icon}
      {children}
    </button>
  );
});
