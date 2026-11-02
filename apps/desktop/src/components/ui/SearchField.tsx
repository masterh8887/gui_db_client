'use client';

import { Search, X } from 'lucide-react';
import { useState, type KeyboardEventHandler, type Ref } from 'react';
import { cn } from '@/lib/utils';
import { KeyHint } from './KeyHint';
import { us } from './strings';

export interface SearchFieldProps {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  /** KEYMAP combo shown while the field is empty and unfocused (`mod+shift+f`). */
  shortcut?: string;
  autoFocus?: boolean;
  inputRef?: Ref<HTMLInputElement>;
  /** Runs first; `preventDefault()` there skips the built-in Esc handling. */
  onKeyDown?: KeyboardEventHandler<HTMLInputElement>;
  'aria-label': string;
  className?: string;
  /** sm 22px (sidebar), md 26px (Welcome header). */
  size?: 'sm' | 'md';
}

/**
 * Search input with a magnifier, a KeyHint while empty and a clear button while filled.
 * Esc with text clears it and is consumed (`preventDefault`); Esc on an empty field passes
 * through to the global shortcuts.
 */
export function SearchField({
  value,
  onChange,
  placeholder,
  shortcut,
  autoFocus,
  inputRef,
  onKeyDown,
  className,
  size = 'sm',
  ...aria
}: SearchFieldProps) {
  const [focused, setFocused] = useState(false);
  return (
    <div
      className={cn(
        'input-focus group relative flex min-w-0 items-center gap-1.5 rounded-md border border-border-input bg-input px-2',
        size === 'sm' ? 'h-(--h-control-sm)' : 'h-(--h-control)',
        className,
      )}
    >
      <Search size={13} strokeWidth={1.75} className="shrink-0 text-fg-tertiary" aria-hidden />
      <input
        ref={inputRef}
        type="search"
        role="searchbox"
        aria-label={aria['aria-label']}
        value={value}
        placeholder={placeholder ?? us('search')}
        autoFocus={autoFocus}
        spellCheck={false}
        autoComplete="off"
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
        onKeyDown={(e) => {
          onKeyDown?.(e);
          if (e.defaultPrevented) return;
          if (e.key === 'Escape' && value) {
            e.preventDefault();
            e.stopPropagation();
            onChange('');
          }
        }}
        className={cn(
          'min-w-0 flex-1 bg-transparent text-fg outline-none placeholder:text-fg-tertiary [&::-webkit-search-cancel-button]:hidden',
          size === 'sm' ? 'text-sm' : 'text-base',
        )}
      />
      {value ? (
        <button
          type="button"
          aria-label={us('clearSearch')}
          tabIndex={-1}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => onChange('')}
          className="inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-fg-tertiary/60 text-window hover:bg-fg-secondary"
        >
          <X size={10} strokeWidth={2.5} />
        </button>
      ) : (
        shortcut && !focused && <KeyHint combo={shortcut} plain />
      )}
    </div>
  );
}
