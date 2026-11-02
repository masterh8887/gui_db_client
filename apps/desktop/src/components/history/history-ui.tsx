'use client';

// Shared by the sidebar history list and the history tab: state icon, search, entry actions.

import type { HistoryEntry, JobState } from 'ipc-types';
import { Ban, Check, CircleDashed, CircleX, TriangleAlert } from 'lucide-react';
import type { KeyboardEvent } from 'react';
import { Spinner, type MenuItem } from '@/components/ui';
import { revealPath } from '@/lib/dialogs';
import { stateLabel } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { isBusyState } from '@/lib/job-state';
import { hasModKey } from '@/lib/platform';
import { cn, copyText } from '@/lib/utils';
import { logLocal } from '@/stores/logs';
import { useTabsStore } from '@/stores/tabs';
import { useWorkspaceStore } from '@/stores/workspace';
import { hs } from './strings';

/** A job state as icon + colour (never colour alone); the accessible name is the state. */
export function HistoryStateIcon({ state, className }: { state: JobState; className?: string }) {
  const label = stateLabel(state);
  const p = { size: 14, strokeWidth: 2, role: 'img' as const, 'aria-label': label };
  if (isBusyState(state)) {
    return (
      <span role="img" aria-label={label} className={cn('inline-flex w-3.5 shrink-0 justify-center', className)}>
        <Spinner size={11} tone="current" />
      </span>
    );
  }
  switch (state) {
    case 'done':
      return <Check {...p} className={cn('sel-muted shrink-0 text-success', className)} />;
    case 'failed':
    case 'rejected':
      return <CircleX {...p} className={cn('sel-muted shrink-0 text-danger', className)} />;
    case 'cancelled':
      return <Ban {...p} className={cn('sel-muted shrink-0 text-warning', className)} />;
    case 'warned':
      return <TriangleAlert {...p} className={cn('sel-muted shrink-0 text-warning', className)} />;
    default:
      return <CircleDashed {...p} className={cn('sel-muted shrink-0 text-fg-tertiary', className)} />;
  }
}

/** Client-side search over the loaded page: every word must appear in name, SQL, files or connection. */
export function filterHistory(entries: readonly HistoryEntry[], query: string, connName: (id: string) => string): HistoryEntry[] {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return entries as HistoryEntry[];
  return entries.filter((h) => {
    const hay = [h.label, h.sql, h.sqlFile ?? '', ...h.files, connName(h.connId)].join('\n').toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

export const entryId = (h: HistoryEntry) => String(h.id);

/** Open the entry in a new query tab (switching workspaces for another connection's entry). */
export function openEntry(h: HistoryEntry): void {
  void useWorkspaceStore.getState().openFromHistory(h);
}

/** Open it in a new query tab and start the normal run flow (preflight first). */
export function runEntryAgain(h: HistoryEntry): void {
  void useWorkspaceStore.getState().openFromHistory(h, { run: true });
}

export function copyEntrySql(h: HistoryEntry): void {
  void copyText(h.sql);
}

/** Reveal the first output file in the OS file manager. */
export function openEntryFolder(h: HistoryEntry): void {
  const file = h.files[0];
  if (!file) return;
  revealPath(file).catch((e: unknown) => logLocal('error', `open_path: ${errorText(e)}`));
}

/** The row / `⋯` menu of an entry. */
export function entryMenu(h: HistoryEntry, opts: { fullHistoryConnId?: string } = {}): MenuItem[] {
  const items: MenuItem[] = [
    { id: 'open', label: hs('open'), shortcut: 'enter', onSelect: () => openEntry(h) },
    { id: 'again', label: hs('runAgain'), shortcut: 'mod+enter', onSelect: () => runEntryAgain(h) },
    { id: 'copy', label: hs('copySql'), shortcut: 'mod+c', onSelect: () => copyEntrySql(h), separatorBefore: true },
    { id: 'folder', label: hs('openFolder'), disabled: h.files.length === 0, onSelect: () => openEntryFolder(h) },
  ];
  const conn = opts.fullHistoryConnId;
  if (conn) items.push({ id: 'full', label: hs('fullHistory'), separatorBefore: true, onSelect: () => useTabsStore.getState().openHistory(conn) });
  return items;
}

/**
 * Keys on a selected entry: `Mod+Enter` runs it again, `Mod+C` copies its SQL (unless page text
 * is selected). `Enter` (open) is the list's own. Consumed keys skip the global shortcuts.
 */
export function entryKeyDown(e: KeyboardEvent, h: HistoryEntry | undefined): void {
  if (!h || !hasModKey(e) || e.altKey || e.shiftKey) return;
  if (e.key === 'Enter') {
    e.preventDefault();
    runEntryAgain(h);
  } else if (e.key.toLowerCase() === 'c' && !window.getSelection()?.toString()) {
    e.preventDefault();
    copyEntrySql(h);
  }
}
