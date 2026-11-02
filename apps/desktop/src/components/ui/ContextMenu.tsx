'use client';

import * as RadixContextMenu from '@radix-ui/react-context-menu';
import { Check } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { KeyHint } from './KeyHint';

export interface MenuItem {
  id: string;
  label: ReactNode;
  icon?: ReactNode;
  onSelect?: () => void;
  disabled?: boolean;
  danger?: boolean;
  /** Renders a separator before this item. */
  separatorBefore?: boolean;
  /** KEYMAP combo shown right-aligned (`mod+w`). */
  shortcut?: string;
  /** Check mark (`kind: 'radio'` or a toggle item). */
  checked?: boolean;
  /** `label`: a non-interactive caption row; `radio`: shows a check when `checked`. */
  kind?: 'item' | 'radio' | 'label';
  /** Tooltip-like reason shown for a disabled item. */
  title?: string;
}

export const menuContentClass =
  'animate-enter z-50 min-w-[200px] max-w-[360px] rounded-[8px] bg-elevated p-1 text-base text-fg shadow-popover';

/** Menu row classes (24px, no hover transition); `danger` paints the text red. */
export function menuItemClass(danger = false): string {
  return cn(
    'flex h-(--h-row-sm) select-none items-center gap-2 rounded-[5px] px-2 outline-none data-[disabled]:opacity-40 data-[highlighted]:bg-selected-focus data-[highlighted]:text-on-accent',
    danger && 'text-danger',
  );
}

export const menuLabelClass = 'flex h-(--h-row-sm) items-center px-2 text-sm font-semibold text-fg-secondary';
export const menuSeparatorClass = 'mx-1 my-1 h-px bg-separator';

/** The inside of one menu row (check / icon column, label, shortcut). Shared with DropdownMenu. */
export function MenuRowContent({ item, hasChecks }: { item: MenuItem; hasChecks: boolean }) {
  return (
    <>
      {hasChecks && (
        <span className="inline-flex w-3.5 shrink-0 justify-center">{item.checked && <Check size={12} strokeWidth={2.25} />}</span>
      )}
      {item.icon && <span className="inline-flex w-3.5 shrink-0 justify-center [&_svg]:size-3.5">{item.icon}</span>}
      <span className="min-w-0 flex-1 truncate">{item.label}</span>
      {item.shortcut && <KeyHint combo={item.shortcut} plain className="ml-4 group-data-[highlighted]:text-on-accent-muted" />}
    </>
  );
}

export function menuHasChecks(items: MenuItem[]): boolean {
  return items.some((i) => i.kind === 'radio' || i.checked !== undefined);
}

export function ContextMenu({
  items,
  children,
  className,
  disabled = false,
}: {
  items: MenuItem[];
  children: ReactNode;
  className?: string;
  /** Render the children without a menu. */
  disabled?: boolean;
}) {
  if (disabled || items.length === 0) return <div className={className}>{children}</div>;
  const hasChecks = menuHasChecks(items);
  return (
    <RadixContextMenu.Root>
      <RadixContextMenu.Trigger asChild>
        <div className={className} data-own-context-menu>
          {children}
        </div>
      </RadixContextMenu.Trigger>
      <RadixContextMenu.Portal>
        <RadixContextMenu.Content className={menuContentClass}>
          {items.map((it) => (
            <div key={it.id}>
              {it.separatorBefore && <RadixContextMenu.Separator className={menuSeparatorClass} />}
              {it.kind === 'label' ? (
                <RadixContextMenu.Label className={menuLabelClass}>{it.label}</RadixContextMenu.Label>
              ) : (
                <RadixContextMenu.Item
                  disabled={it.disabled}
                  onSelect={it.onSelect}
                  title={it.title}
                  className={cn('group', menuItemClass(it.danger))}
                >
                  <MenuRowContent item={it} hasChecks={hasChecks} />
                </RadixContextMenu.Item>
              )}
            </div>
          ))}
        </RadixContextMenu.Content>
      </RadixContextMenu.Portal>
    </RadixContextMenu.Root>
  );
}
