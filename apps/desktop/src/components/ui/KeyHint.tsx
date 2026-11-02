'use client';

import type { ReactNode } from 'react';
import { comboLabel } from '@/lib/keymap';
import { useIsMac } from '@/lib/platform';
import { cn } from '@/lib/utils';

/**
 * Keyboard hint for a KEYMAP combo: `mod+shift+enter` → `⌘⇧↩` on macOS, `Ctrl+Shift+Enter` on
 * Windows (DESIGN.md §6). 11px Latin, radius 4. `plain` drops the chip (menus).
 */
export function KeyHint({ combo, className, plain = false }: { combo: string; className?: string; plain?: boolean }) {
  const mac = useIsMac();
  return (
    <kbd
      className={cn(
        'inline-flex shrink-0 items-center whitespace-nowrap font-sans text-xs tabular-nums sel-muted',
        plain ? 'text-fg-tertiary' : 'h-4 rounded-sm bg-control px-1 text-fg-secondary',
        className,
      )}
    >
      {comboLabel(combo, mac)}
    </kbd>
  );
}

/** Older keyboard hint taking free text; renders like `KeyHint`. `inverse` sits on a primary button. */
export function Kbd({ children, className, inverse = false }: { children: ReactNode; className?: string; inverse?: boolean }) {
  return (
    <kbd
      className={cn(
        'inline-flex h-4 shrink-0 items-center whitespace-nowrap rounded-sm px-1 font-sans text-xs tabular-nums',
        inverse ? 'bg-white/20 text-on-accent' : 'bg-control text-fg-secondary',
        className,
      )}
    >
      {children}
    </kbd>
  );
}
