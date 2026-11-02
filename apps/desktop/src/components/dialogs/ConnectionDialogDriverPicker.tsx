'use client';

// Step 1 of a new connection (DESIGN.md §5.B): a searchable 3-column grid of driver tiles.
// Unavailable drivers are dimmed with "설치 필요" but stay selectable (saving is blocked later,
// with the install hint). Arrows move (↑↓ by a row), Enter / double-click go to the form.

import type { DriverInfo } from 'ipc-types';
import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { fuzzyMatch } from '@/lib/fuzzy';
import { cn } from '@/lib/utils';
import { EmptyState, SearchField } from '@/components/ui';
import { DriverBadge, tierLabel } from '@/components/welcome/DriverBadge';
import { cs } from './ConnectionDialog.strings';

const COLUMNS = 3;

function matchesDriver(d: DriverInfo, q: string): boolean {
  if (!q) return true;
  return [d.name, d.id, tierLabel(d.tier)].some((s) => fuzzyMatch(q, s) !== null);
}

export function DriverPicker({
  drivers,
  value,
  onChange,
  onPick,
}: {
  drivers: readonly DriverInfo[];
  /** Selected driver id. */
  value: string | null;
  onChange: (id: string) => void;
  /** Enter / double-click: continue to the form with this driver. */
  onPick: (id: string) => void;
}) {
  const [query, setQuery] = useState('');
  const gridRef = useRef<HTMLDivElement>(null);
  const prefix = useId();
  const shown = useMemo(() => drivers.filter((d) => matchesDriver(d, query.trim())), [drivers, query]);
  const index = shown.findIndex((d) => d.id === value);
  const tileId = (i: number) => `${prefix}-drv-${i}`;

  useEffect(() => {
    if (index >= 0) document.getElementById(`${prefix}-drv-${index}`)?.scrollIntoView?.({ block: 'nearest' });
  }, [index, prefix]);

  const onQuery = (q: string) => {
    setQuery(q);
    const next = drivers.filter((d) => matchesDriver(d, q.trim()));
    if (next.length > 0 && !next.some((d) => d.id === value)) onChange(next[0].id);
  };

  const move = (to: number) => {
    const d = shown[Math.max(0, Math.min(shown.length - 1, to))];
    if (d) onChange(d.id);
  };

  const onGridKey = (e: KeyboardEvent) => {
    if (shown.length === 0) return;
    const at = index < 0 ? 0 : index;
    switch (e.key) {
      case 'ArrowRight':
        move(index < 0 ? 0 : at + 1);
        break;
      case 'ArrowLeft':
        move(index < 0 ? 0 : at - 1);
        break;
      case 'ArrowDown':
        move(index < 0 ? 0 : at + COLUMNS < shown.length ? at + COLUMNS : at);
        break;
      case 'ArrowUp':
        move(index < 0 ? 0 : at - COLUMNS >= 0 ? at - COLUMNS : at);
        break;
      case 'Home':
        move(0);
        break;
      case 'End':
        move(shown.length - 1);
        break;
      case 'Enter':
        if (index >= 0) onPick(shown[index].id);
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (index < 0) move(0);
      gridRef.current?.focus();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const d = index >= 0 ? shown[index] : shown[0];
      if (d) onPick(d.id);
    }
  };

  return (
    <div className="flex flex-col gap-3">
      <SearchField
        size="md"
        autoFocus
        value={query}
        onChange={onQuery}
        onKeyDown={onSearchKey}
        placeholder={cs('driverSearch')}
        aria-label={cs('driverSearch')}
      />
      {shown.length === 0 ? (
        <EmptyState compact title={cs('noDriver')} />
      ) : (
        <div
          ref={gridRef}
          role="listbox"
          aria-label={cs('driverList')}
          aria-orientation="horizontal"
          tabIndex={0}
          data-roving-list=""
          aria-activedescendant={index >= 0 ? tileId(index) : undefined}
          onKeyDown={onGridKey}
          className="grid grid-cols-3 gap-1 outline-none"
        >
          {shown.map((d, i) => (
            <div
              key={d.id}
              id={tileId(i)}
              role="option"
              aria-selected={i === index}
              data-driver={d.id}
              onClick={() => onChange(d.id)}
              onDoubleClick={() => onPick(d.id)}
              className="flex h-16 min-w-0 items-center gap-2.5 rounded-md px-2.5 not-aria-selected:hover:bg-hover aria-selected:bg-selected"
            >
              <DriverBadge driverId={d.id} name={d.name} size="lg" className={cn(!d.available && 'opacity-50')} />
              <div className={cn('min-w-0 flex-1', !d.available && 'opacity-60')}>
                <div className="truncate font-medium">{d.name}</div>
                {d.available ? (
                  <div className="sel-muted truncate text-sm text-fg-secondary">{tierLabel(d.tier)}</div>
                ) : (
                  // Tier and "설치 필요" on their own lines: a 64px tile fits three, and neither
                  // Hangul caption is cut in a 170px tile.
                  <div className="sel-muted min-w-0 text-sm text-fg-secondary" title={`${tierLabel(d.tier)} · ${cs('needsInstall')}`}>
                    <div className="truncate">{tierLabel(d.tier)}</div>
                    <div className="truncate">{cs('needsInstall')}</div>
                  </div>
                )}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
