import { cn } from '@/lib/utils';

/** Spinning ring. `current` takes the colour of the surrounding text (inside badges). */
export function Spinner({ size = 14, className, tone = 'accent' }: { size?: number; className?: string; tone?: 'accent' | 'current' }) {
  return (
    <span
      role="status"
      aria-label="loading"
      className={cn(
        'inline-block shrink-0 animate-spin rounded-full border-2',
        tone === 'current' ? 'border-current/40 border-t-current' : 'border-muted-foreground/40 border-t-accent',
        className,
      )}
      style={{ width: size, height: size }}
    />
  );
}
