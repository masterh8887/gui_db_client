'use client';

// Driver badge (DESIGN.md §5.A): a 28px rounded square with a two-letter driver code, tinted
// with the connection's environment tag. Inside a focused selected row it switches to the tag's
// solid fill with white text (§3.3 "Selection").

import type { DriverTier } from 'ipc-types';
import type { CSSProperties } from 'react';
import type { EnvTag } from '@/lib/env';
import { cn } from '@/lib/utils';
import { ws } from './strings';

const CODES: Record<string, string> = {
  postgres: 'Pg',
  postgresql: 'Pg',
  mysql: 'My',
  mariadb: 'My',
  mssql: 'Ms',
  sqlserver: 'Ms',
  sqlite: 'Lt',
  oracle: 'Or',
  odbc: 'Od',
};

/** `Pg My Ms Lt Or Od`, else the first two letters of the driver's name (`Cl` for ClickHouse). */
export function driverCode(driverId: string, name?: string): string {
  const known = CODES[driverId.toLowerCase()];
  if (known) return known;
  const letters = (name || driverId).replace(/[^\p{L}\p{N}]/gu, '');
  if (!letters) return '?';
  return letters.charAt(0).toUpperCase() + letters.charAt(1).toLowerCase();
}

/** 내장 / 벤더 클라이언트 / ODBC / 플러그인. */
export function tierLabel(tier: DriverTier): string {
  switch (tier) {
    case 'builtin':
      return ws('tierBuiltin');
    case 'vendor-client':
      return ws('tierVendor');
    case 'odbc':
      return ws('tierOdbc');
    case 'plugin':
      return ws('tierPlugin');
  }
}

/** Badge colours as custom properties, so the focused-selection variant can swap them in CSS. */
function badgeVars(tag: EnvTag | undefined): CSSProperties {
  if (!tag) {
    return {
      '--badge-bg': 'var(--bg-control)',
      '--badge-fg': 'var(--fg-secondary)',
      '--badge-fill': 'color-mix(in srgb, var(--fg-on-accent) 22%, transparent)',
    } as CSSProperties;
  }
  return {
    '--badge-bg': `var(--env-${tag}-soft)`,
    '--badge-fg': `var(--env-${tag})`,
    '--badge-fill': `var(--env-${tag}-fill)`,
  } as CSSProperties;
}

export function DriverBadge({
  driverId,
  name,
  tag,
  size = 'md',
  className,
}: {
  driverId: string;
  /** `DriverInfo.name` (fallback code for unknown drivers). */
  name?: string;
  tag?: EnvTag;
  /** sm 22px (form driver row), md 28px (rows), lg 32px (driver tiles). */
  size?: 'sm' | 'md' | 'lg';
  className?: string;
}) {
  return (
    <span
      aria-hidden
      style={badgeVars(tag)}
      className={cn(
        'inline-flex shrink-0 items-center justify-center rounded-md bg-(--badge-bg) font-semibold tracking-tight text-(--badge-fg) tabular-nums',
        'in-[[data-roving-list]:focus-within_[aria-selected=true]]:bg-(--badge-fill) in-[[data-roving-list]:focus-within_[aria-selected=true]]:text-on-accent',
        size === 'lg' ? 'h-8 w-8 text-sm' : size === 'sm' ? 'h-[22px] w-[22px] text-xs' : 'h-7 w-7 text-xs',
        className,
      )}
    >
      {driverCode(driverId, name)}
    </span>
  );
}
