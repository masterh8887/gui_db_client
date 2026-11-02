'use client';

import { useEffect, useRef, useState } from 'react';
import { cn } from '@/lib/utils';

/**
 * Drag handle on one edge of a panel: a 1px hairline with a 6px hit area. `edge` is the side
 * of the panel the handle sits on (a right-edge handle grows the panel when dragged right).
 */
export function ResizeHandle({
  edge,
  size,
  onResize,
  label,
}: {
  edge: 'left' | 'right' | 'top';
  size: number;
  onResize: (px: number) => void;
  label: string;
}) {
  const [drag, setDrag] = useState<{ start: number; size: number } | null>(null);
  const latest = useRef(onResize);
  useEffect(() => {
    latest.current = onResize;
  });
  const horizontal = edge !== 'top';

  useEffect(() => {
    if (!drag) return;
    const onMove = (e: MouseEvent) => {
      const pos = horizontal ? e.clientX : e.clientY;
      const delta = pos - drag.start;
      latest.current(edge === 'right' ? drag.size + delta : drag.size - delta);
    };
    const onUp = () => setDrag(null);
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
    const prev = document.body.style.cursor;
    document.body.style.cursor = horizontal ? 'col-resize' : 'row-resize';
    return () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      document.body.style.cursor = prev;
    };
  }, [drag, edge, horizontal]);

  return (
    <div
      role="separator"
      aria-label={label}
      aria-orientation={horizontal ? 'vertical' : 'horizontal'}
      aria-valuenow={Math.round(size)}
      onMouseDown={(e) => {
        e.preventDefault();
        setDrag({ start: horizontal ? e.clientX : e.clientY, size });
      }}
      className={cn(
        'absolute z-20',
        edge === 'right' && 'top-0 -right-[3px] bottom-0 w-1.5 cursor-col-resize',
        edge === 'left' && 'top-0 -left-[3px] bottom-0 w-1.5 cursor-col-resize',
        edge === 'top' && '-top-[3px] right-0 left-0 h-1.5 cursor-row-resize',
        drag && 'bg-accent/40',
      )}
    />
  );
}
