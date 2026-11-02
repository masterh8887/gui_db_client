'use client';

import { ChevronDown, CircleAlert } from 'lucide-react';
import { forwardRef, type ReactNode } from 'react';
import { comboLabel } from '@/lib/keymap';
import type { EnvTag as EnvTagValue } from '@/lib/env';
import { cn } from '@/lib/utils';
import type { MenuItem } from './ContextMenu';
import { DropdownMenu } from './DropdownMenu';
import { EnvTag } from './EnvTag';
import { SegmentedControl } from './SegmentedControl';
import { Spinner } from './Spinner';
import { us } from './strings';
import { Tooltip } from './Tooltip';

export interface ToolbarProps {
  left?: ReactNode;
  center?: ReactNode;
  right?: ReactNode;
  className?: string;
  /** macOS Overlay title bar: empty toolbar space drags the window. */
  dragRegion?: boolean;
}

/** 40px toolbar, CSS grid `1fr auto 1fr` so the centre is truly centred (DESIGN.md §5.C). */
export function Toolbar({ left, center, right, className, dragRegion = false }: ToolbarProps) {
  const drag = dragRegion ? { 'data-tauri-drag-region': '' } : {};
  return (
    <div
      role="toolbar"
      {...drag}
      className={cn('grid h-(--h-toolbar) shrink-0 grid-cols-[1fr_auto_1fr] items-center gap-2 border-b border-separator bg-toolbar px-2', className)}
    >
      <div {...drag} className="flex min-w-0 items-center gap-0.5 justify-self-start">
        {left}
      </div>
      <div className="flex min-w-0 items-center justify-center">{center}</div>
      <div {...drag} className="flex min-w-0 items-center gap-0.5 justify-self-end">
        {right}
      </div>
    </div>
  );
}

export interface ToolbarButtonProps {
  icon: ReactNode;
  label: string;
  /** KEYMAP combo shown in the tooltip: "label (⌘P)". */
  shortcut?: string;
  onClick?: () => void;
  active?: boolean;
  disabled?: boolean;
  loading?: boolean;
  showLabel?: boolean;
  /** Turns the button into a menu trigger (with a chevron). */
  menu?: MenuItem[];
  className?: string;
}

/** 28px ghost toolbar button with a 16px icon; tooltip after 300ms. */
export const ToolbarButton = forwardRef<HTMLButtonElement, ToolbarButtonProps>(function ToolbarButton(
  { icon, label, shortcut, onClick, active = false, disabled = false, loading = false, showLabel = false, menu, className },
  ref,
) {
  const tip = shortcut ? `${label} (${comboLabel(shortcut)})` : label;
  const button = (
    <button
      ref={ref}
      type="button"
      aria-label={label}
      aria-pressed={menu ? undefined : active || undefined}
      aria-busy={loading || undefined}
      disabled={disabled}
      onClick={menu ? undefined : onClick}
      className={cn(
        'inline-flex h-7 shrink-0 items-center justify-center gap-1 rounded-md transition-colors duration-[80ms] hover:bg-hover active:bg-pressed disabled:pointer-events-none disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-focus [&_svg]:shrink-0',
        showLabel || menu ? 'px-1.5' : 'w-7',
        active ? 'bg-control text-accent' : 'text-fg-secondary hover:text-fg',
        className,
      )}
    >
      {loading ? <Spinner size={14} tone="current" decorative /> : <span className="inline-flex [&_svg]:size-4">{icon}</span>}
      {showLabel && <span className="text-base">{label}</span>}
      {menu && <ChevronDown size={11} strokeWidth={2} className="text-fg-tertiary" />}
    </button>
  );
  if (menu) {
    return (
      <Tooltip content={tip} delay={300}>
        <span className="inline-flex">
          <DropdownMenu items={menu} align="start">
            {button}
          </DropdownMenu>
        </span>
      </Tooltip>
    );
  }
  return (
    <Tooltip content={tip} delay={300}>
      {button}
    </Tooltip>
  );
});

export interface StatusPillProps {
  tag?: EnvTagValue;
  name: string;
  /** Text after the name (`PostgreSQL 16.2 : sales : public`, or the active job's progress). */
  detail: ReactNode;
  state: 'idle' | 'connecting' | 'ok' | 'error';
  /** A 2px line along the bottom: 0..100, `indeterminate`, or none. */
  progress?: number | 'indeterminate' | null;
  onClick?: () => void;
  className?: string;
}

/**
 * The centred status pill (DESIGN.md §5.C): `TAG │ name : detail`. Tinted with the tag's soft
 * colour (no tag: control grey). Connecting shows a spinner and "연결 중…", an error the
 * `CircleAlert` icon and "연결 실패" (never a bare red dot).
 */
export const StatusPill = forwardRef<HTMLButtonElement, StatusPillProps>(function StatusPill(
  { tag, name, detail, state, progress, onClick, className },
  ref,
) {
  const after =
    state === 'connecting' ? (
      <span className="flex min-w-0 items-center gap-1.5 text-fg-secondary">
        <Spinner size={11} tone="current" decorative />
        {us('connecting')}
      </span>
    ) : state === 'error' ? (
      <span className="flex min-w-0 items-center gap-1 text-danger">
        <CircleAlert size={13} strokeWidth={2} />
        {us('connectFailed')}
      </span>
    ) : (
      <span className="min-w-0 truncate text-fg-secondary">{detail}</span>
    );
  return (
    <button
      ref={ref}
      type="button"
      onClick={onClick}
      aria-haspopup="dialog"
      className={cn(
        'relative flex h-7 max-w-[520px] min-w-0 items-center gap-2 overflow-hidden rounded-full pr-3 pl-1 text-base outline-none hover:brightness-[0.98] focus-visible:outline-2 focus-visible:outline-focus',
        !tag && 'pl-3',
        className,
      )}
      style={{ background: tag ? `var(--env-${tag}-soft)` : 'var(--bg-control)' }}
    >
      {tag && <EnvTag tag={tag} variant="chip" />}
      {tag && <span aria-hidden className="h-3.5 w-px shrink-0 bg-separator-strong" />}
      <span className="max-w-[200px] shrink-0 truncate font-medium text-fg">{name}</span>
      <span aria-hidden className="shrink-0 text-fg-tertiary">:</span>
      {after}
      {progress !== undefined && progress !== null && (
        <span aria-hidden className="absolute inset-x-3 bottom-0 h-0.5 overflow-hidden rounded-full">
          {progress === 'indeterminate' ? (
            <span className="animate-indeterminate absolute inset-y-0 w-[35%] rounded-full bg-accent" />
          ) : (
            <span className="block h-full rounded-full bg-accent" style={{ width: `${Math.max(0, Math.min(100, progress))}%` }} />
          )}
        </span>
      )}
    </button>
  );
});

export interface InspectorPanelProps {
  segment: string;
  onSegmentChange: (s: string) => void;
  segments: { value: string; label: ReactNode; badge?: ReactNode }[];
  children: ReactNode;
  'aria-label'?: string;
}

/** Right sidebar body: a 28px tabs header over the content. */
export function InspectorPanel({ segment, onSegmentChange, segments, children, ...aria }: InspectorPanelProps) {
  return (
    <div data-surface="sidebar" className="flex h-full min-h-0 flex-col bg-sidebar">
      <div className="flex h-(--h-bar) shrink-0 items-stretch border-b border-separator px-3">
        <SegmentedControl
          variant="tabs"
          aria-label={aria['aria-label'] ?? us('inspector')}
          value={segment}
          onChange={onSegmentChange}
          options={segments.map((s) => ({ value: s.value, label: s.label, badge: s.badge }))}
        />
      </div>
      <div className="min-h-0 flex-1 overflow-auto">{children}</div>
    </div>
  );
}

/**
 * Inner 28px bar (sidebar bottom bar, table tab bar, query bar, console header). Not an
 * app-wide status bar: the app has none (DESIGN.md §4).
 *
 * Surface (DESIGN.md §3.1: `--bg-panel` is for the export panel, console, query bar and table
 * headers only): `auto` paints `bg-panel`, except inside a `data-surface="sidebar"` container
 * (SidebarFrame, InspectorPanel) where it stays transparent so the sidebar reads as one surface;
 * `panel` / `inherit` force either.
 */
export function StatusBar({
  children,
  className,
  position = 'bottom',
  surface = 'auto',
}: {
  children: ReactNode;
  className?: string;
  /** Which edge carries the hairline. */
  position?: 'top' | 'bottom';
  surface?: 'auto' | 'panel' | 'inherit';
}) {
  return (
    <div
      className={cn(
        'flex h-(--h-bar) shrink-0 items-center gap-2 px-2 text-sm',
        surface === 'inherit' ? 'bg-transparent' : 'bg-panel',
        surface === 'auto' && '[[data-surface=sidebar]_&]:bg-transparent',
        position === 'bottom' ? 'border-t border-separator' : 'border-b border-separator',
        className,
      )}
    >
      {children}
    </div>
  );
}
