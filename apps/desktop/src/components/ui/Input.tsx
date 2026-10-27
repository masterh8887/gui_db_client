'use client';

import { forwardRef, type InputHTMLAttributes, type ReactNode, type TextareaHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

const inputBase =
  'w-full min-w-0 rounded border bg-background px-2 text-foreground placeholder:text-muted-foreground/70 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50 read-only:bg-muted';

/**
 * Classes for text-like controls. `cn` does not resolve Tailwind conflicts (no tailwind-merge),
 * so size and state are chosen here instead of being overridden through `className`.
 */
export function inputClasses({ dense = false, invalid = false, multiline = false } = {}): string {
  return cn(
    inputBase,
    multiline ? 'py-1 leading-5' : dense ? 'h-6' : 'h-7',
    dense ? 'text-xs' : 'text-[13px]',
    invalid ? 'border-danger focus:border-danger' : 'border-border focus:border-accent',
  );
}

export interface InputProps extends InputHTMLAttributes<HTMLInputElement> {
  invalid?: boolean;
  /** 24px tall, 12px text (toolbars). */
  dense?: boolean;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input({ className, invalid, dense, ...rest }, ref) {
  return (
    <input
      ref={ref}
      className={cn(inputClasses({ dense, invalid }), className)}
      aria-invalid={invalid || undefined}
      spellCheck={false}
      autoComplete="off"
      {...rest}
    />
  );
});

export interface TextareaProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  invalid?: boolean;
}

export const Textarea = forwardRef<HTMLTextAreaElement, TextareaProps>(function Textarea({ className, invalid, ...rest }, ref) {
  return (
    <textarea
      ref={ref}
      className={cn(inputClasses({ invalid, multiline: true }), className)}
      aria-invalid={invalid || undefined}
      spellCheck={false}
      {...rest}
    />
  );
});

/** Label + control + optional hint, laid out for the dense forms used everywhere. */
export function Field({
  label,
  htmlFor,
  hint,
  error,
  className,
  children,
  inline = false,
}: {
  label: ReactNode;
  htmlFor?: string;
  hint?: ReactNode;
  error?: ReactNode;
  className?: string;
  children: ReactNode;
  /** Label on the left (checkbox / switch rows). */
  inline?: boolean;
}) {
  if (inline) {
    return (
      <label htmlFor={htmlFor} className={cn('flex h-7 cursor-pointer items-center gap-2 text-[13px]', className)}>
        {children}
        <span>{label}</span>
        {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
      </label>
    );
  }
  return (
    <div className={cn('flex flex-col gap-1', className)}>
      <label htmlFor={htmlFor} className="text-xs font-medium text-muted-foreground">
        {label}
      </label>
      {children}
      {error ? (
        <span className="text-xs text-danger">{error}</span>
      ) : hint ? (
        <span className="text-xs text-muted-foreground">{hint}</span>
      ) : null}
    </div>
  );
}
