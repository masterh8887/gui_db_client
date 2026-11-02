'use client';

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  type ReactNode,
  type RefObject,
} from 'react';
import { createPortal } from 'react-dom';
import { cn } from '@/lib/utils';

export interface PopoverProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Element the popover hangs from (its own clicks never count as "outside"). */
  anchor: RefObject<HTMLElement | null>;
  align?: 'start' | 'center' | 'end';
  side?: 'top' | 'bottom';
  /** Fixed width (px); otherwise the content decides. */
  width?: number;
  /** Move focus inside on open and back to the anchor on close (default true). */
  initialFocus?: boolean;
  children: ReactNode;
  'aria-label': string;
  className?: string;
}

/** Nested popovers register their element so clicks inside them stay "inside" the parent. */
const PopoverParentContext = createContext<((el: HTMLElement) => () => void) | null>(null);

const FOCUSABLE = '[autofocus], input:not([disabled]), textarea:not([disabled]), select:not([disabled]), button:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';
const GAP = 6;
const MARGIN = 8;

/**
 * Hand-built popover (DESIGN.md §7.4; `@radix-ui/react-popover` is not a dependency): a portal
 * with role=dialog aria-modal=false. Closes on a pointerdown outside (not inside the anchor, a
 * Radix popper it opened, or a nested popover), on Esc while focus is inside (consumed), and
 * when the anchor unmounts.
 */
export function Popover(props: PopoverProps) {
  if (!props.open || typeof document === 'undefined') return null;
  return <PopoverSurface {...props} />;
}

function PopoverSurface({
  onOpenChange,
  anchor,
  align = 'start',
  side = 'bottom',
  width,
  initialFocus = true,
  children,
  className,
  ...aria
}: PopoverProps) {
  const ref = useRef<HTMLDivElement>(null);
  const nested = useRef(new Set<HTMLElement>());
  const registerWithParent = useContext(PopoverParentContext);
  const close = useRef(onOpenChange);
  useEffect(() => {
    close.current = onOpenChange;
  });

  const register = useCallback((el: HTMLElement) => {
    nested.current.add(el);
    return () => {
      nested.current.delete(el);
    };
  }, []);

  useEffect(() => {
    if (registerWithParent && ref.current) return registerWithParent(ref.current);
  }, [registerWithParent]);

  // Position next to the anchor, clamped to the viewport; follows resizes and scrolling.
  useLayoutEffect(() => {
    const place = () => {
      const a = anchor.current;
      const el = ref.current;
      if (!a || !el) return;
      const r = a.getBoundingClientRect();
      const w = width ?? el.offsetWidth;
      const h = el.offsetHeight;
      let left = align === 'start' ? r.left : align === 'end' ? r.right - w : r.left + r.width / 2 - w / 2;
      let top = side === 'bottom' ? r.bottom + GAP : r.top - GAP - h;
      if (side === 'bottom' && top + h > window.innerHeight - MARGIN && r.top - GAP - h >= MARGIN) top = r.top - GAP - h;
      left = Math.max(MARGIN, Math.min(left, window.innerWidth - w - MARGIN));
      top = Math.max(MARGIN, Math.min(top, window.innerHeight - h - MARGIN));
      el.style.left = `${Math.round(left)}px`;
      el.style.top = `${Math.round(top)}px`;
      el.style.visibility = 'visible';
    };
    place();
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
    };
  }, [anchor, align, side, width]);

  // Outside pointerdown closes.
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      const t = e.target;
      if (!(t instanceof Node)) return;
      if (ref.current?.contains(t)) return;
      if (anchor.current?.contains(t)) return;
      if (t instanceof Element && t.closest('[data-radix-popper-content-wrapper]')) return;
      for (const el of nested.current) if (el.contains(t)) return;
      close.current(false);
    };
    document.addEventListener('pointerdown', onDown, true);
    return () => document.removeEventListener('pointerdown', onDown, true);
  }, [anchor]);

  // Focus in on open, back to the anchor on close (only when focus was still inside).
  useEffect(() => {
    if (!initialFocus) return;
    const el = ref.current;
    const target = el?.querySelector<HTMLElement>(FOCUSABLE) ?? el;
    target?.focus();
    const anchorEl = anchor.current;
    return () => {
      const active = document.activeElement;
      if (!active || active === document.body || el?.contains(active)) anchorEl?.focus?.();
    };
  }, [initialFocus, anchor]);

  // The anchor went away: close.
  useEffect(() => {
    if (!anchor.current || !anchor.current.isConnected) close.current(false);
  });

  return createPortal(
    <PopoverParentContext.Provider value={register}>
      <div
        ref={ref}
        role="dialog"
        aria-modal="false"
        aria-label={aria['aria-label']}
        tabIndex={-1}
        onKeyDown={(e) => {
          if (e.key === 'Escape' && !e.defaultPrevented) {
            e.preventDefault();
            e.stopPropagation();
            close.current(false);
          }
        }}
        style={{ position: 'fixed', top: 0, left: 0, width, visibility: 'hidden' }}
        className={cn('animate-enter z-50 overflow-hidden rounded-lg bg-elevated text-base text-fg shadow-popover outline-none', className)}
      >
        {children}
      </div>
    </PopoverParentContext.Provider>,
    document.body,
  );
}
