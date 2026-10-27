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
  /** Hide the divider and freeze the size (used for the collapsed log bar). */
  disabled?: boolean;
  className?: string;
}

/**
 * Two-child resizable split. The divider is dragged with the mouse; sizes are clamped to
 * `[min, max]` and to the container, and reported through `onResize` so the owner (ui store)
 * can persist them.
 */
export function SplitPane({ direction, sized, size, min, max, onResize, children, disabled = false, className }: SplitPaneProps) {
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
      const upper = Math.min(max, Math.max(min, total - 48));
      onResize(Math.max(min, Math.min(upper, raw)));
    };
    const onUp = () => setDragging(false);
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    const prevCursor = document.body.style.cursor;
    const prevSelect = document.body.style.userSelect;
    document.body.style.cursor = horizontal ? 'col-resize' : 'row-resize';
    document.body.style.userSelect = 'none';
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.style.cursor = prevCursor;
      document.body.style.userSelect = prevSelect;
    };
  }, [dragging, horizontal, sized, min, max, onResize]);

  const sizedStyle = horizontal ? { width: size, minWidth: size } : { height: size, minHeight: size };
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
        'relative z-10 shrink-0 transition-colors',
        horizontal ? 'w-px' : 'h-px',
        disabled ? 'cursor-default' : horizontal ? 'cursor-col-resize' : 'cursor-row-resize',
        dragging ? 'bg-accent' : 'bg-border',
      )}
    >
      {/* Wider invisible hit area. */}
      {!disabled && <div className={cn('absolute', horizontal ? '-left-1 -right-1 top-0 bottom-0' : '-top-1 -bottom-1 left-0 right-0')} />}
    </div>
  );

  return (
    <div ref={containerRef} className={cn('flex h-full w-full min-h-0 min-w-0', horizontal ? 'flex-row' : 'flex-col', className)}>
      <div className={cn('min-h-0 min-w-0 overflow-hidden', sized === 'first' ? 'shrink-0' : 'flex-1')} style={sized === 'first' ? sizedStyle : undefined}>
        {first}
      </div>
      {divider}
      <div className={cn('min-h-0 min-w-0 overflow-hidden', sized === 'second' ? 'shrink-0' : 'flex-1')} style={sized === 'second' ? sizedStyle : undefined}>
        {second}
      </div>
    </div>
  );
}
