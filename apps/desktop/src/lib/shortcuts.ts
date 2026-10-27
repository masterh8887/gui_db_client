'use client';

// Global keyboard shortcuts: Ctrl/Cmd+Enter run, Ctrl/Cmd+E explain, Esc cancel (asks).
// Keys pressed inside an open dialog belong to that dialog and never reach these bindings.

import { useEffect, useRef } from 'react';
import { hasModKey } from './platform';

export interface ShortcutBinding {
  /** `KeyboardEvent.key` value (`Enter`, `e`, `Escape`); matched case-insensitively. */
  key: string;
  /** Requires the primary modifier (Cmd on macOS, Ctrl elsewhere). */
  mod?: boolean;
  shift?: boolean;
  /** Fire even when focus is inside an input/textarea/contenteditable (default: only with `mod`). */
  inInputs?: boolean;
  /** Fire even when focus is inside an open dialog (default: never). */
  inDialogs?: boolean;
  handler: (e: KeyboardEvent) => void;
}

function inDialog(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('[role="dialog"],[role="alertdialog"]') !== null;
}

function isEditable(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
}

/** Does `e` match `b`? Exported so the matcher can be unit tested without React. */
export function matchesShortcut(e: KeyboardEvent, b: ShortcutBinding): boolean {
  if (e.key.toLowerCase() !== b.key.toLowerCase()) return false;
  const wantMod = !!b.mod;
  if (wantMod !== hasModKey(e)) return false;
  if (!!b.shift !== e.shiftKey) return false;
  if (e.altKey) return false;
  if (!wantMod && !b.inInputs && isEditable(e.target)) return false;
  if (!b.inDialogs && inDialog(e.target)) return false;
  return true;
}

/**
 * Attach `bindings` to `window` for the life of the component. The latest bindings are always
 * used (stored in a ref), so callers can pass inline closures.
 */
export function useGlobalShortcuts(bindings: ShortcutBinding[], enabled = true): void {
  const ref = useRef(bindings);
  useEffect(() => {
    ref.current = bindings;
  });
  useEffect(() => {
    if (!enabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      for (const b of ref.current) {
        if (matchesShortcut(e, b)) {
          e.preventDefault();
          b.handler(e);
          return;
        }
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [enabled]);
}
