'use client';

import * as RadixDropdown from '@radix-ui/react-dropdown-menu';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import {
  MenuRowContent,
  menuContentClass,
  menuHasChecks,
  menuItemClass,
  menuLabelClass,
  menuSeparatorClass,
  type MenuItem,
} from './ContextMenu';

export function DropdownMenu({
  items,
  children,
  align = 'end',
  side = 'bottom',
  open,
  onOpenChange,
}: {
  items: MenuItem[];
  /** The trigger element (`asChild`). */
  children: ReactNode;
  align?: 'start' | 'center' | 'end';
  side?: 'top' | 'bottom';
  /** Controlled open state (optional). */
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
}) {
  const hasChecks = menuHasChecks(items);
  return (
    <RadixDropdown.Root open={open} onOpenChange={onOpenChange}>
      <RadixDropdown.Trigger asChild>{children}</RadixDropdown.Trigger>
      <RadixDropdown.Portal>
        <RadixDropdown.Content align={align} side={side} sideOffset={4} collisionPadding={8} className={menuContentClass}>
          {items.map((it) => (
            <div key={it.id}>
              {it.separatorBefore && <RadixDropdown.Separator className={menuSeparatorClass} />}
              {it.kind === 'label' ? (
                <RadixDropdown.Label className={menuLabelClass}>{it.label}</RadixDropdown.Label>
              ) : (
                <RadixDropdown.Item
                  disabled={it.disabled}
                  onSelect={it.onSelect}
                  title={it.title}
                  className={cn('group', menuItemClass(it.danger))}
                >
                  <MenuRowContent item={it} hasChecks={hasChecks} />
                </RadixDropdown.Item>
              )}
            </div>
          ))}
        </RadixDropdown.Content>
      </RadixDropdown.Portal>
    </RadixDropdown.Root>
  );
}
