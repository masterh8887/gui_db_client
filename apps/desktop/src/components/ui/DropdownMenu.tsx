'use client';

import * as RadixDropdown from '@radix-ui/react-dropdown-menu';
import type { ReactNode } from 'react';
import { menuContentClass, menuItemClass, type MenuItem } from './ContextMenu';

export function DropdownMenu({
  items,
  children,
  align = 'end',
}: {
  items: MenuItem[];
  /** The trigger element (`asChild`). */
  children: ReactNode;
  align?: 'start' | 'end';
}) {
  return (
    <RadixDropdown.Root>
      <RadixDropdown.Trigger asChild>{children}</RadixDropdown.Trigger>
      <RadixDropdown.Portal>
        <RadixDropdown.Content align={align} sideOffset={4} className={menuContentClass}>
          {items.map((it) => (
            <div key={it.id}>
              {it.separatorBefore && <RadixDropdown.Separator className="my-1 h-px bg-border" />}
              <RadixDropdown.Item
                disabled={it.disabled}
                onSelect={it.onSelect}
                className={menuItemClass(it.danger)}
              >
                {it.icon && <span className="inline-flex w-4 justify-center">{it.icon}</span>}
                {it.label}
              </RadixDropdown.Item>
            </div>
          ))}
        </RadixDropdown.Content>
      </RadixDropdown.Portal>
    </RadixDropdown.Root>
  );
}
