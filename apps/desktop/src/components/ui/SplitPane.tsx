'use client';

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface SplitPaneProps {
  direction: 'horizontal' | 'vertical';
  /** Which child has the fixed `size`; the other one fills the rest. */
  sized: 'first' | 'second';
  /** Size (px) of the sized child. */
  size: number;
  min: number;
  max: number;
  onResize: (size: number) => void;
  children: [ReactNode, ReactNode];
  /** Hide the divider and freeze the size. */
  disabled?: boolean;
  /** Upper bound as a fraction of the container (export panel: 0.5). */
  maxFraction?: number;
  className?: string;
}

/**
 * Two-child resizable split: a 1px hairline with a 6px hit area. Sizes are clamped to
 * `[min, max]` (and `maxFraction` of the container) and reported through `onResize`, so the
 * owner (ui store) can persist them.
 */
export function SplitPane({ direction, sized, size, min, max, onResize, children, disabled = false, maxFraction, className }: SplitPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [dragging, setDragging] = useState(false);
  const horizontal = direction === 'horizontal';

  const onMouseDown = useCallback(
    (e: React.MouseEvent) => {
      if (disabled) return;
      e.preventDefault();
      setDragging(true);
    },
    [disabled],
  );

  useEffect(() => {
    if (!dragging) return;
    const onMove = (e: MouseEvent) => {
      const el = containerRef.current;
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const total = horizontal ? rect.width : rect.height;
      const fromStart = horizontal ? e.clientX - rect.left : e.clientY - rect.top;
      const raw = sized === 'first' ? fromStart : total - fromStart;
      let upper = Math.min(max, Math.max(min, total - 48));
      if (maxFraction) upper = Math.min(upper, Math.max(min, total * maxFraction));
      onResize(Math.max(min, Math.min(upper, raw)));
    };
    const onUp = () => setDragging(false);
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    const prevCursor = document.body.style.cursor;
    document.body.style.cursor = horizontal ? 'col-resize' : 'row-resize';
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.style.cursor = prevCursor;
    };
  }, [dragging, horizontal, sized, min, max, maxFraction, onResize]);

  const sizedStyle = horizontal ? { width: size, minWidth: size } : { height: size, minHeight: size };
  const fractionStyle = maxFraction ? (horizontal ? { maxWidth: `${maxFraction * 100}%` } : { maxHeight: `${maxFraction * 100}%` }) : {};
  const [first, second] = children;

  const divider = (
    <div
      role="separator"
      aria-orientation={horizontal ? 'vertical' : 'horizontal'}
      aria-valuenow={size}
      aria-valuemin={min}
      aria-valuemax={max}
      onMouseDown={onMouseDown}
      className={cn(
        'relative z-10 shrink-0',
        horizontal ? 'w-px' : 'h-px',
        disabled ? '' : horizontal ? 'cursor-col-resize' : 'cursor-row-resize',
        dragging ? 'bg-accent' : 'bg-separator',
      )}
    >
      {/* 6px invisible hit area. */}
      {!disabled && (
        <div className={cn('absolute', horizontal ? '-left-[3px] -right-[3px] top-0 bottom-0' : '-top-[3px] -bottom-[3px] left-0 right-0')} />
      )}
    </div>
  );

  return (
    <div ref={containerRef} className={cn('flex h-full w-full min-h-0 min-w-0', horizontal ? 'flex-row' : 'flex-col', className)}>
      <div
        className={cn('min-h-0 min-w-0 overflow-hidden', sized === 'first' ? 'shrink-0' : 'flex-1')}
        style={sized === 'first' ? { ...sizedStyle, ...fractionStyle } : undefined}
      >
        {first}
      </div>
      {divider}
      <div
        className={cn('min-h-0 min-w-0 overflow-hidden', sized === 'second' ? 'shrink-0' : 'flex-1')}
        style={sized === 'second' ? { ...sizedStyle, ...fractionStyle } : undefined}
      >
        {second}
      </div>
    </div>
  );
}
