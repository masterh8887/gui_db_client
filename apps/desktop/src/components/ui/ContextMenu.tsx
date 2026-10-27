'use client';

import * as RadixContextMenu from '@radix-ui/react-context-menu';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface MenuItem {
  id: string;
  label: ReactNode;
  icon?: ReactNode;
  onSelect?: () => void;
  disabled?: boolean;
  danger?: boolean;
  /** Renders a separator before this item. */
  separatorBefore?: boolean;
}

export const menuContentClass =
  'z-50 min-w-[180px] rounded border border-border bg-background p-1 text-[13px] shadow-lg';
/** Menu row classes; `danger` swaps the colours (chosen here: `cn` does not merge conflicts). */
export function menuItemClass(danger = false): string {
  return cn(
    'flex h-7 cursor-default select-none items-center gap-2 rounded px-2 outline-none data-[disabled]:opacity-40',
    danger
      ? 'text-danger data-[highlighted]:bg-danger data-[highlighted]:text-white'
      : 'data-[highlighted]:bg-accent data-[highlighted]:text-accent-foreground',
  );
}

export function ContextMenu({ items, children, className }: { items: MenuItem[]; children: ReactNode; className?: string }) {
  return (
    <RadixContextMenu.Root>
      <RadixContextMenu.Trigger asChild>
        <div className={className}>{children}</div>
      </RadixContextMenu.Trigger>
      <RadixContextMenu.Portal>
        <RadixContextMenu.Content className={menuContentClass}>
          {items.map((it) => (
            <div key={it.id}>
              {it.separatorBefore && <RadixContextMenu.Separator className="my-1 h-px bg-border" />}
              <RadixContextMenu.Item
                disabled={it.disabled}
                onSelect={it.onSelect}
                className={menuItemClass(it.danger)}
              >
                {it.icon && <span className="inline-flex w-4 justify-center">{it.icon}</span>}
                {it.label}
              </RadixContextMenu.Item>
            </div>
          ))}
        </RadixContextMenu.Content>
      </RadixContextMenu.Portal>
    </RadixContextMenu.Root>
  );
}
