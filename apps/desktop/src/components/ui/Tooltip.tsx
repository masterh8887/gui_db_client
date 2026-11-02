'use client';

import * as RadixTooltip from '@radix-ui/react-tooltip';
import { useRef, useState, type ReactNode } from 'react';

/** Wrap the app once; individual `Tooltip`s need it. Radix default delay (DESIGN.md §3.3). */
export function TooltipProvider({ children }: { children: ReactNode }) {
  return (
    <RadixTooltip.Provider delayDuration={600} skipDelayDuration={300}>
      {children}
    </RadixTooltip.Provider>
  );
}

export interface TooltipProps {
  content: ReactNode;
  children: ReactNode;
  side?: 'top' | 'right' | 'bottom' | 'left';
  /** Override the open delay (toolbar: 300ms). */
  delay?: number;
}

function matches(el: Element, selector: string): boolean {
  try {
    return el.matches(selector);
  } catch {
    return false; // a test DOM without the pseudo-class
  }
}

/** The trigger (or the button inside it) has its menu open. */
function menuOpen(el: HTMLElement | null): boolean {
  return !!el && (el.getAttribute('aria-expanded') === 'true' || el.querySelector('[aria-expanded="true"]') !== null);
}

/**
 * Tooltip with native manners: it hides on press and stays hidden until the pointer leaves
 * (a menu trigger's `preventDefault` on pointerdown would otherwise keep it open over the menu),
 * never shows while the trigger's menu is open, and does not pop up when a closing menu or
 * dialog hands focus back to the trigger (only keyboard focus, `:focus-visible`, shows it).
 */
export function Tooltip({ content, children, side = 'bottom', delay }: TooltipProps) {
  const [open, setOpen] = useState(false);
  const pressed = useRef(false);
  // Radix types the trigger ref as a button; with `asChild` it is whatever element the child renders.
  const triggerRef = useRef<HTMLButtonElement>(null);
  if (!content) return <>{children}</>;

  const onOpenChange = (next: boolean) => {
    if (next) {
      const el = triggerRef.current;
      if (pressed.current || menuOpen(el)) return;
      const active = typeof document !== 'undefined' ? document.activeElement : null;
      if (el && active instanceof HTMLElement && el.contains(active) && !matches(el, ':hover') && !matches(active, ':focus-visible')) return;
    }
    setOpen(next);
  };

  return (
    <RadixTooltip.Root open={open} onOpenChange={onOpenChange} delayDuration={delay}>
      <RadixTooltip.Trigger
        asChild
        ref={triggerRef}
        onPointerDownCapture={() => {
          pressed.current = true;
          setOpen(false);
        }}
        onPointerLeave={() => {
          pressed.current = false;
        }}
        onKeyDownCapture={(e) => {
          if (e.key === 'Enter' || e.key === ' ' || e.key === 'ArrowDown') setOpen(false);
        }}
      >
        {children}
      </RadixTooltip.Trigger>
      <RadixTooltip.Portal>
        <RadixTooltip.Content
          side={side}
          sideOffset={5}
          collisionPadding={8}
          className="animate-fade z-[60] max-w-xs whitespace-pre-line rounded-[6px] bg-elevated px-2 py-1 text-sm text-fg shadow-popover"
        >
          {content}
        </RadixTooltip.Content>
      </RadixTooltip.Portal>
    </RadixTooltip.Root>
  );
}
