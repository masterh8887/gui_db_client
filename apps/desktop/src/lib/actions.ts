'use client';

// Action registry (DESIGN.md §7.5): every command the app can run, for the palette, the keymap
// dispatcher, the macOS menu (`menu:action` → `runAction`) and buttons. `registerBuiltinActions`
// (idempotent; AppShell and palette tests call it) adds the built-in ids of §7.5.

import { logLocal } from '@/stores/logs';
import { actionRegistry as registry, actionRegistryState, resetActionRegistry } from './action-registry';
import { builtinActions } from './builtin-actions';
import { errorText } from './ipc-error';
import { ls } from './strings';
import { toast } from './toast';

export type ActionGroup = 'query' | 'tabs' | 'view' | 'catalog' | 'connection' | 'jobs' | 'app';

export interface AppAction {
  id: string;
  group: ActionGroup;
  title: () => string;
  keywords?: string[];
  /** KEYMAP combo shown as a KeyHint (`mod+enter`). */
  shortcut?: string;
  /** Disabled actions are skipped by `runAction` and hidden in the palette. */
  enabled?: () => boolean;
  /** Not listed in the palette (tab 1…8 and similar keyboard-only actions). */
  hidden?: boolean;
  run: () => void | Promise<void>;
}


/** Every registered action, in registration order. */
export function getActions(): AppAction[] {
  return [...registry.values()];
}

export function getAction(id: string): AppAction | undefined {
  return registry.get(id);
}

/** Whether `id` exists and is enabled right now. */
export function isActionEnabled(id: string): boolean {
  const a = registry.get(id);
  return !!a && (a.enabled?.() ?? true);
}

/**
 * Run an action by id. Unknown or disabled ids do nothing; a failure is logged and toasted
 * (never thrown).
 */
export async function runAction(id: string): Promise<void> {
  const a = registry.get(id);
  if (!a) {
    logLocal('warn', `Unknown action: ${id}`);
    return;
  }
  if (a.enabled && !a.enabled()) return;
  try {
    await a.run();
  } catch (e) {
    const message = ls('actionFailed', { action: a.title(), error: errorText(e) });
    logLocal('error', message);
    toast(message, { tone: 'danger' });
  }
}

/** Add (or replace) actions; the returned function removes exactly these again. */
export function registerActions(actions: AppAction[]): () => void {
  for (const a of actions) registry.set(a.id, a);
  return () => {
    for (const a of actions) if (registry.get(a.id) === a) registry.delete(a.id);
  };
}

/** Register the built-in actions once. */
export function registerBuiltinActions(): void {
  if (actionRegistryState.builtinsRegistered) return;
  actionRegistryState.builtinsRegistered = true;
  registerActions(builtinActions());
}

export function __resetForTests(): void {
  resetActionRegistry();
}
