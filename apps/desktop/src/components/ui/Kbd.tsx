import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

/** Keyboard hint. `inverse` is for use on an accent-coloured (primary) button. */
export function Kbd({ children, className, inverse = false }: { children: ReactNode; className?: string; inverse?: boolean }) {
  return (
    <kbd
      className={cn(
        'inline-flex h-[18px] items-center rounded-sm border px-1 font-sans text-[11px]',
        inverse ? 'border-accent-foreground/40 bg-transparent text-accent-foreground' : 'border-border bg-muted text-muted-foreground',
        className,
      )}
    >
      {children}
    </kbd>
  );
}
