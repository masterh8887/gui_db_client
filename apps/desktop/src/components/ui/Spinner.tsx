import { cn } from '@/lib/utils';
import { us } from './strings';

export interface SpinnerProps {
  size?: number;
  className?: string;
  /** `current` takes the colour of the surrounding text (inside chips, buttons). */
  tone?: 'accent' | 'current';
  /** Accessible name of the `status` (default: "불러오는 중…" / "Loading…"). */
  label?: string;
  /**
   * Purely visual (`aria-hidden`, no `status` role): for a spinner next to its own text or inside
   * a control that already carries a label or `aria-busy`.
   */
  decorative?: boolean;
}

/** Spinning ring. A `status` with a localised name unless `decorative`. */
export function Spinner({ size = 14, className, tone = 'accent', label, decorative = false }: SpinnerProps) {
  return (
    <span
      {...(decorative ? { 'aria-hidden': true } : { role: 'status', 'aria-label': label ?? us('loading') })}
      className={cn(
        'inline-block shrink-0 animate-spin rounded-full border-[1.5px]',
        tone === 'current' ? 'border-current/30 border-t-current' : 'border-fg-tertiary/35 border-t-accent',
        className,
      )}
      style={{ width: size, height: size }}
    />
  );
}
