'use client';

import { ChevronRight, Ellipsis } from 'lucide-react';
import { useId, type ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { ContextMenu, type MenuItem } from './ContextMenu';
import { DropdownMenu } from './DropdownMenu';
import { us } from './strings';
import { Tooltip } from './Tooltip';

export interface SidebarSectionProps {
  id: string;
  title: ReactNode;
  count?: number;
  /** Right-aligned caption (the sticky `추정` label). */
  caption?: ReactNode;
  collapsed: boolean;
  onToggle: () => void;
  /** Hover actions on the header. */
  actions?: ReactNode;
  children: ReactNode;
}

/**
 * Sidebar section: a 24px sticky header (12px/600 secondary caption + count, chevron on hover)
 * over its rows (DESIGN.md §5.C "Sidebar").
 */
export function SidebarSection({ id, title, count, caption, collapsed, onToggle, actions, children }: SidebarSectionProps) {
  const headerId = `${useId()}-${id}-header`;
  return (
    <div role="group" aria-labelledby={headerId} data-section={id}>
      <div className="group/section sticky top-0 z-10 flex h-(--h-row-sm) items-center gap-1 bg-sidebar pr-2 pl-2">
        <button
          type="button"
          id={headerId}
          aria-expanded={!collapsed}
          onClick={onToggle}
          className="flex min-w-0 flex-1 items-center gap-1 rounded-sm text-left text-sm font-semibold text-fg-secondary outline-none focus-visible:outline-2 focus-visible:outline-focus"
        >
          <span className="truncate">{title}</span>
          {count !== undefined && <span className="text-xs font-normal tabular-nums text-fg-tertiary">{count}</span>}
          <ChevronRight
            size={12}
            strokeWidth={2}
            aria-hidden
            className={cn(
              'shrink-0 text-fg-tertiary opacity-0 group-hover/section:opacity-100 group-focus-within/section:opacity-100',
              !collapsed && 'rotate-90',
              collapsed && 'opacity-100',
            )}
          />
        </button>
        {actions && <div className="flex items-center opacity-0 group-hover/section:opacity-100 group-focus-within/section:opacity-100">{actions}</div>}
        {caption && <span className="shrink-0 text-sm text-fg-secondary">{caption}</span>}
      </div>
      {!collapsed && children}
    </div>
  );
}

export interface SidebarItemProps {
  /** DOM id (for `aria-activedescendant`). */
  id: string;
  icon?: ReactNode;
  label: ReactNode;
  /** Right-aligned secondary text (`≈48.2M`). */
  trailing?: ReactNode;
  selected?: boolean;
  /** Preview tab target: italic label. */
  preview?: boolean;
  muted?: boolean;
  depth?: number;
  /** Tooltip. */
  title?: string;
  onClick?: () => void;
  onDoubleClick?: () => void;
  /** Context menu and the hover `⋯` button. */
  contextItems?: MenuItem[];
  'aria-label'?: string;
  className?: string;
}

/**
 * 26px list row (role=option in a listbox; selection per DESIGN.md §3.3: `--bg-selected`, and
 * accent fill with white text while the list has focus). Not focusable itself: the listbox owns
 * focus and points at the row with `aria-activedescendant`.
 */
export function SidebarItem({
  id,
  icon,
  label,
  trailing,
  selected = false,
  preview = false,
  muted = false,
  depth = 0,
  title,
  onClick,
  onDoubleClick,
  contextItems,
  className,
  ...aria
}: SidebarItemProps) {
  const row = (
    <div
      id={id}
      role="option"
      aria-selected={selected}
      aria-label={aria['aria-label']}
      onClick={onClick}
      onDoubleClick={onDoubleClick}
      style={{ paddingLeft: 8 + depth * 14 }}
      className={cn(
        'group/item relative flex h-(--h-row) select-none items-center gap-1.5 rounded-md pr-1.5 outline-none',
        selected ? 'bg-selected' : 'hover:bg-hover',
        muted ? 'text-fg-secondary' : 'text-fg',
        className,
      )}
    >
      {icon && <span className="sel-muted inline-flex shrink-0 text-fg-secondary [&_svg]:size-3.5">{icon}</span>}
      <span className={cn('min-w-0 flex-1 truncate', preview && 'italic')}>{label}</span>
      {trailing !== undefined && trailing !== null && (
        <span className="sel-muted shrink-0 text-xs tabular-nums text-fg-secondary group-hover/item:hidden">{trailing}</span>
      )}
      {contextItems && contextItems.length > 0 && (
        <DropdownMenu items={contextItems} align="start">
          <button
            type="button"
            tabIndex={-1}
            aria-label={us('moreActions')}
            onClick={(e) => e.stopPropagation()}
            onDoubleClick={(e) => e.stopPropagation()}
            className="sel-muted hidden h-5 w-5 shrink-0 items-center justify-center rounded-[4px] text-fg-secondary hover:bg-pressed group-hover/item:inline-flex"
          >
            <Ellipsis size={14} strokeWidth={1.75} />
          </button>
        </DropdownMenu>
      )}
    </div>
  );
  const withTip = title ? (
    <Tooltip content={title} side="right">
      {row}
    </Tooltip>
  ) : (
    row
  );
  return contextItems && contextItems.length > 0 ? <ContextMenu items={contextItems}>{withTip}</ContextMenu> : withTip;
}
