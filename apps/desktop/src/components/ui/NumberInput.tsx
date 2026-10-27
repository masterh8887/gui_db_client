'use client';

import { useState, type InputHTMLAttributes } from 'react';
import { cn } from '@/lib/utils';
import { inputClasses } from './Input';

export interface NumberInputProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'value' | 'onChange' | 'type'> {
  value: number | undefined;
  /** `undefined` when the field is cleared (optional settings). */
  onChange: (value: number | undefined) => void;
  min?: number;
  max?: number;
  step?: number;
  integer?: boolean;
  invalid?: boolean;
}

/**
 * Numeric input that keeps the raw text while typing and only reports valid numbers. Clearing
 * reports `undefined` so optional options can be unset.
 */
export function NumberInput({
  value,
  onChange,
  min,
  max,
  step = 1,
  integer = true,
  invalid,
  className,
  ...rest
}: NumberInputProps) {
  const [text, setText] = useState(value === undefined ? '' : String(value));
  // Follow external value changes (presets, "run again") without clobbering in-progress typing.
  const [prevValue, setPrevValue] = useState(value);
  if (prevValue !== value) {
    setPrevValue(value);
    const current = text.trim() === '' ? undefined : Number(text);
    if (current !== value) setText(value === undefined ? '' : String(value));
  }

  const commit = (raw: string) => {
    setText(raw);
    if (raw.trim() === '') {
      onChange(undefined);
      return;
    }
    const n = Number(raw);
    if (!Number.isFinite(n)) return;
    if (integer && !Number.isInteger(n)) return;
    if (min !== undefined && n < min) return;
    if (max !== undefined && n > max) return;
    onChange(n);
  };

  const bad = text.trim() !== '' && (!Number.isFinite(Number(text)) || (integer && !Number.isInteger(Number(text))));
  return (
    <input
      type="number"
      inputMode={integer ? 'numeric' : 'decimal'}
      className={cn(inputClasses({ invalid: invalid || bad }), 'tabular-nums', className)}
      value={text}
      min={min}
      max={max}
      step={step}
      onChange={(e) => commit(e.target.value)}
      aria-invalid={invalid || bad || undefined}
      {...rest}
    />
  );
}
