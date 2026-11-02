'use client';

// The app keymap dispatcher (DESIGN.md §6): every KEYMAP row of scope `always`, `workspace` and
// `queryTarget` becomes a window shortcut that runs its action. Welcome rows are labels only
// (WelcomeScreen binds those keys itself). Rows owned by the macOS menu are skipped inside
// Tauri on macOS, where the menu fires them as `menu:action`.

import { runAction } from '@/lib/actions';
import { inTauri } from '@/lib/ipc';
import { bindingFor, KEYMAP } from '@/lib/keymap';
import { isMac } from '@/lib/platform';
import { editableHasText, isEditable, useGlobalShortcuts, type ShortcutBinding } from '@/lib/shortcuts';
import { selectActiveQueryTarget, useTabsStore } from '@/stores/tabs';
import type { AppMode } from '@/stores/workspace';

/**
 * Esc fires only when nothing local wants it (DESIGN.md §6 "Esc precedence"): never from a
 * completion popup; in the SQL editor whatever it holds (CodeMirror's own keymap already
 * consumed the Esc it wanted: completion close, selection collapse); in other inputs only when
 * empty. CodeMirror is decided first: its `.cm-content` is contenteditable and is never empty
 * to `textContent` (the SQL, or the placeholder widget).
 */
export function escapeAllowed(e: KeyboardEvent): boolean {
  const t = e.target;
  if (t instanceof Element) {
    if (t.closest('.cm-tooltip-autocomplete')) return false;
    if (t.closest('.cm-editor')) return true;
  }
  if (isEditable(t) && editableHasText(t)) return false;
  return true;
}

/** The ShortcutBindings for a mode (exported for tests). */
export function workspaceBindings(mode: AppMode, connId: string | null, run: (action: string) => void): ShortcutBinding[] {
  const skipMenu = inTauri() && isMac();
  const out: ShortcutBinding[] = [];
  for (const row of KEYMAP) {
    if (row.scope === 'welcome') continue;
    if (row.menuOwned && skipMenu) continue;
    if (row.scope === 'workspace' && mode !== 'workspace') continue;
    if (row.scope === 'queryTarget') {
      if (mode !== 'workspace') continue;
    }
    const b = bindingFor(row, () => run(row.action));
    if (row.scope === 'queryTarget') {
      b.when = () => selectActiveQueryTarget(useTabsStore.getState(), connId) !== null;
    }
    if (row.action === 'jobs.cancelAsk' && row.combo === 'escape') b.when = escapeAllowed;
    out.push(b);
  }
  return out;
}

/** Mount once (AppShell). */
export function useWorkspaceShortcuts(mode: AppMode, connId: string | null): void {
  useGlobalShortcuts(workspaceBindings(mode, connId, (action) => void runAction(action)));
}
