'use client';

import { forwardRef, type InputHTMLAttributes, type ReactNode, type TextareaHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';

const inputBase =
  'input-focus min-w-0 rounded-md border bg-input px-2 text-fg placeholder:text-fg-tertiary disabled:pointer-events-none disabled:opacity-50 read-only:bg-control';

/**
 * True when the caller's classes set a width (`w-24`, `w-[76px]`). Without tailwind-merge two
 * width utilities race on CSS order (`w-full` won), so controls drop their default `w-full`.
 */
export function hasWidthClass(className: string | undefined): boolean {
  return !!className && /(^|\s)w-/.test(className);
}

/**
 * Classes for text-like controls: 26px (`dense` 22px), `--bg-input`, 1px `--border-input`,
 * radius 6, accent border + 3px ring on focus (DESIGN.md §3.3). `cn` does not resolve Tailwind
 * conflicts (no tailwind-merge), so size and state are chosen here, not overridden by callers;
 * the width is the exception (`fullWidth: false` when the caller sets one, see hasWidthClass).
 */
export function inputClasses({ dense = false, invalid = false, multiline = false, fullWidth = true } = {}): string {
  return cn(
    inputBase,
    fullWidth && 'w-full',
    multiline ? 'py-1 leading-5' : dense ? 'h-(--h-control-sm)' : 'h-(--h-control)',
    dense ? 'text-sm' : 'text-base',
    invalid ? 'border-danger' : 'border-border-input',
  );
}

export interface InputProps extends InputHTMLAttributes<HTMLInputElement> {
  invalid?: boolean;
  /** 22px tall, 12px text (bars). */
  dense?: boolean;
}

export const Input = forwardRef<HTMLInputElement, InputProps>(function Input({ className, invalid, dense, ...rest }, ref) {
  return (
    <input
      ref={ref}
      className={cn(inputClasses({ dense, invalid, fullWidth: !hasWidthClass(className) }), className)}
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
      className={cn(inputClasses({ invalid, multiline: true, fullWidth: !hasWidthClass(className) }), className)}
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
  /** Label on the right of the control (checkbox / switch rows). */
  inline?: boolean;
}) {
  if (inline) {
    return (
      <label htmlFor={htmlFor} className={cn('flex min-h-(--h-control) items-center gap-2 text-base', className)}>
        {children}
        <span>{label}</span>
        {hint && <span className="text-sm text-fg-secondary">{hint}</span>}
      </label>
    );
  }
  return (
    <div className={cn('flex flex-col gap-1', className)}>
      <label htmlFor={htmlFor} className="text-sm font-medium text-fg-secondary">
        {label}
      </label>
      {children}
      {error ? (
        <span className="text-sm text-danger">{error}</span>
      ) : hint ? (
        <span className="text-sm text-fg-secondary">{hint}</span>
      ) : null}
    </div>
  );
}
