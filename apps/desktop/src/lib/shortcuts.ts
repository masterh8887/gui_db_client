'use client';

// Keyboard shortcut matching (DESIGN.md §6 "Matching"). The app-wide map lives in `./keymap`
// and is dispatched by the layout (`useWorkspaceShortcuts`); components with local keys (the
// Welcome list, dialogs) use `useGlobalShortcuts` directly.
//
// Rules:
// - A binding matches on `e.key` (case-insensitive) or on the physical `e.code`, so layouts that
//   type another character on the same key (macOS Korean: `` ` `` and `\` type `₩`) still work.
// - Events with `isComposing` / `keyCode 229` (IME) are ignored, except Escape.
// - `mod` is Cmd on macOS and Ctrl elsewhere. `ctrl` is the physical Ctrl key: it requires
//   `e.ctrlKey` and, off macOS, also satisfies `mod`. On macOS a binding without `ctrl`
//   requires `!e.ctrlKey`. `shift` and `alt` must match exactly.
// - Keys pressed inside an open dialog (`role=dialog`, which includes the palette and a popover
//   with focus inside) never reach a binding unless it sets `inDialogs`.
// - Without a modifier a binding skips editable targets unless it sets `inInputs`.
// - WebView accelerators (reload, print) are always consumed by `useWebViewAcceleratorGuard`,
//   also inside dialogs and while an IME composes; the dispatcher still runs their actions.

import { useEffect, useRef } from 'react';
import { isMac } from './platform';

export interface ShortcutBinding {
  /** `KeyboardEvent.key` value (`Enter`, `e`, `Escape`, `\`); matched case-insensitively. */
  key: string;
  /** `KeyboardEvent.code` alternative (`Backquote`, `Backslash`, `Digit1`, `KeyP`, ...). */
  code?: string;
  /** Requires the primary modifier (Cmd on macOS, Ctrl elsewhere). */
  mod?: boolean;
  /** Requires the physical Ctrl key (off macOS this also counts as `mod`). */
  ctrl?: boolean;
  shift?: boolean;
  alt?: boolean;
  /** Fire even when focus is inside an input/textarea/contenteditable (default: only with a modifier). */
  inInputs?: boolean;
  /** Fire even when focus is inside an open dialog (default: never). */
  inDialogs?: boolean;
  /** Extra condition checked after the key matched (e.g. "the input is empty"). */
  when?: (e: KeyboardEvent) => boolean;
  handler: (e: KeyboardEvent) => void;
}

export function inDialog(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('[role="dialog"],[role="alertdialog"]') !== null;
}

export function isEditable(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
}

/** Text of a contenteditable without `aria-hidden` decorations (CodeMirror's placeholder widget). */
function visibleText(node: Node): string {
  let out = '';
  for (const child of Array.from(node.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) out += child.textContent ?? '';
    else if (child instanceof Element && child.getAttribute('aria-hidden') !== 'true') out += visibleText(child);
  }
  return out;
}

/**
 * Whether an editable target currently holds text (inputs: value; contenteditable: its text,
 * ignoring `aria-hidden` widgets such as CodeMirror's placeholder).
 */
export function editableHasText(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) return target.value.length > 0;
  if (target.isContentEditable) return visibleText(target).length > 0;
  return false;
}

/**
 * Keys the WebView itself acts on (WebView2 leaves browser accelerators on): F5 with any
 * modifier (reload / hard reload), Mod+R and Mod+Shift+R (reload), Mod+P and Mod+Shift+P (print).
 */
export function isWebViewAccelerator(e: KeyboardEvent): boolean {
  if (e.altKey) return false;
  if (e.key === 'F5' || e.code === 'F5') return true;
  const mod = isMac() ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  if (!mod) return false;
  const key = (e.key ?? '').toLowerCase();
  return key === 'r' || e.code === 'KeyR' || key === 'p' || e.code === 'KeyP';
}

/** Events whose default the guard (not a component) prevented: the dispatcher still runs them. */
const guardedEvents = new WeakSet<Event>();

/** Whether a component already consumed `e` (the accelerator guard's own `preventDefault` aside). */
export function consumedLocally(e: KeyboardEvent): boolean {
  return e.defaultPrevented && !guardedEvents.has(e);
}

/**
 * DESIGN.md §6: F5 / Mod+R / Mod+Shift+R / Mod+P never reload or print the WebView, wherever
 * focus is (dialogs, sheets, popovers, the palette, an IME composition). A capture-phase window
 * listener calls `preventDefault()` first; actions still run only where their bindings allow
 * (never inside a dialog). Mount once (AppShell).
 *
 * `nativeMenuOwnsModKeys` (checked per event): true inside Tauri on macOS, where ⌘R / ⌘P are
 * menu items. WKWebView hands a Cmd key to the menu only when the page did not prevent it, and
 * it has no reload / print keys of its own, so the guard leaves Cmd combos alone there.
 */
export function useWebViewAcceleratorGuard(nativeMenuOwnsModKeys: () => boolean = () => false): void {
  const menuRef = useRef(nativeMenuOwnsModKeys);
  useEffect(() => {
    menuRef.current = nativeMenuOwnsModKeys;
  });
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!isWebViewAccelerator(e) || e.defaultPrevented) return;
      if ((e.metaKey || e.ctrlKey) && menuRef.current()) return;
      guardedEvents.add(e);
      e.preventDefault();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, []);
}

/** Does `e` match `b`? Exported so the matcher can be unit tested without React. */
export function matchesShortcut(e: KeyboardEvent, b: ShortcutBinding): boolean {
  const composing = e.isComposing || e.keyCode === 229;
  if (composing && e.key !== 'Escape') return false;
  const keyOk = (e.key ?? '').toLowerCase() === b.key.toLowerCase() || (!!b.code && e.code === b.code);
  if (!keyOk) return false;

  const wantMod = !!b.mod;
  const wantCtrl = !!b.ctrl;
  if (isMac()) {
    if (wantMod !== e.metaKey) return false;
    if (wantCtrl !== e.ctrlKey) return false;
  } else {
    if ((wantMod || wantCtrl) !== e.ctrlKey) return false;
    if (e.metaKey) return false;
  }
  if (!!b.shift !== e.shiftKey) return false;
  if (!!b.alt !== e.altKey) return false;
  if (!wantMod && !wantCtrl && !b.inInputs && isEditable(e.target)) return false;
  if (!b.inDialogs && inDialog(e.target)) return false;
  if (b.when && !b.when(e)) return false;
  return true;
}

/**
 * Attach `bindings` to `window` for the life of the component. The latest bindings are always
 * used (stored in a ref), so callers can pass inline closures. Events another handler already
 * consumed (`defaultPrevented`, except by the accelerator guard) are skipped; a match calls
 * `preventDefault()`.
 */
export function useGlobalShortcuts(bindings: ShortcutBinding[], enabled = true): void {
  const ref = useRef(bindings);
  useEffect(() => {
    ref.current = bindings;
  });
  useEffect(() => {
    if (!enabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (consumedLocally(e)) return;
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
