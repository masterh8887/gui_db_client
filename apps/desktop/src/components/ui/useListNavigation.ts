'use client';

import { useEffect, useId, type KeyboardEvent, type KeyboardEventHandler } from 'react';
import { hasModKey } from '@/lib/platform';

export interface ListNavigationOptions {
  count: number;
  /** Selected row (-1 = none). */
  activeIndex: number;
  /** Move the selection (arrows, Home, End, click). Never loads anything. */
  onMove: (i: number) => void;
  /** Enter (and double-click through `getItemProps`). */
  onOpen?: (i: number) => void;
  /** Delete or Mod+Backspace. */
  onDelete?: (i: number) => void;
}

export interface ListNavigation {
  /** Attach to the focusable list element (the listbox). */
  onKeyDown: KeyboardEventHandler;
  /** Spread on each row: `id`, `role=option`, `aria-selected`, click / double-click. */
  getItemProps(i: number): {
    id: string;
    role: 'option';
    'aria-selected': boolean;
    onClick: () => void;
    onDoubleClick: () => void;
  };
  /** Spread on the list element: role, tabIndex, `aria-activedescendant`, `onKeyDown`. */
  listProps: {
    role: 'listbox';
    tabIndex: number;
    'aria-activedescendant': string | undefined;
    'data-roving-list': '';
    onKeyDown: KeyboardEventHandler;
  };
  /** DOM id of row `i`. */
  itemId(i: number): string;
}

/**
 * Roving selection for lists (Welcome, sidebar, palette): ↑ ↓ Home End move, Enter opens,
 * Delete / Mod+Backspace delete. The list keeps focus; `aria-activedescendant` points at the
 * selected row, which is scrolled into view.
 */
export function useListNavigation({ count, activeIndex, onMove, onOpen, onDelete }: ListNavigationOptions): ListNavigation {
  const prefix = useId();
  const itemId = (i: number) => `${prefix}-opt-${i}`;

  useEffect(() => {
    if (activeIndex < 0) return;
    const el = document.getElementById(`${prefix}-opt-${activeIndex}`);
    el?.scrollIntoView?.({ block: 'nearest' });
  }, [activeIndex, prefix]);

  const onKeyDown = (e: KeyboardEvent) => {
    if (count === 0) return;
    let next: number | null = null;
    switch (e.key) {
      case 'ArrowDown':
        next = activeIndex < 0 ? 0 : Math.min(count - 1, activeIndex + 1);
        break;
      case 'ArrowUp':
        next = activeIndex < 0 ? count - 1 : Math.max(0, activeIndex - 1);
        break;
      case 'Home':
        next = 0;
        break;
      case 'End':
        next = count - 1;
        break;
      case 'Enter':
        if (onOpen && activeIndex >= 0) {
          e.preventDefault();
          onOpen(activeIndex);
        }
        return;
      case 'Delete':
        if (onDelete && activeIndex >= 0) {
          e.preventDefault();
          onDelete(activeIndex);
        }
        return;
      case 'Backspace':
        if (onDelete && activeIndex >= 0 && hasModKey(e)) {
          e.preventDefault();
          onDelete(activeIndex);
        }
        return;
      default:
        return;
    }
    e.preventDefault();
    if (next !== activeIndex) onMove(next);
  };

  return {
    onKeyDown,
    itemId,
    getItemProps: (i: number) => ({
      id: itemId(i),
      role: 'option' as const,
      'aria-selected': i === activeIndex,
      onClick: () => onMove(i),
      onDoubleClick: () => onOpen?.(i),
    }),
    listProps: {
      role: 'listbox' as const,
      tabIndex: 0,
      'aria-activedescendant': activeIndex >= 0 && activeIndex < count ? itemId(activeIndex) : undefined,
      'data-roving-list': '' as const,
      onKeyDown,
    },
  };
}
