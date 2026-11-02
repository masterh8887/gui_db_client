import { renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  editableHasText,
  isWebViewAccelerator,
  matchesShortcut,
  useGlobalShortcuts,
  useWebViewAcceleratorGuard,
  type ShortcutBinding,
} from './shortcuts';

const noop = () => undefined;
const run: ShortcutBinding = { key: 'Enter', mod: true, handler: noop };
const esc: ShortcutBinding = { key: 'Escape', inInputs: true, handler: noop };
const plain: ShortcutBinding = { key: 'x', handler: noop };
const backquote: ShortcutBinding = { key: '`', code: 'Backquote', ctrl: true, inInputs: true, handler: noop };
const inspector: ShortcutBinding = { key: '\\', code: 'Backslash', mod: true, handler: noop };

function keydown(key: string, target: Element, init: KeyboardEventInit = {}): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, ...init });
  Object.defineProperty(e, 'target', { value: target });
  return e;
}

const mac = () => vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
const win = () => vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');

describe('matchesShortcut', () => {
  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  it('requires the platform modifier (Ctrl off macOS)', () => {
    win();
    const body = document.body;
    expect(matchesShortcut(keydown('Enter', body, { ctrlKey: true }), run)).toBe(true);
    expect(matchesShortcut(keydown('Enter', body, { metaKey: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Enter', body), run)).toBe(false);
    expect(matchesShortcut(keydown('Enter', body, { ctrlKey: true, altKey: true }), run)).toBe(false);
  });

  it('uses Cmd on macOS', () => {
    mac();
    expect(matchesShortcut(keydown('Enter', document.body, { metaKey: true }), run)).toBe(true);
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true }), run)).toBe(false);
    // A binding without `ctrl` refuses a held Ctrl on macOS.
    expect(matchesShortcut(keydown('Enter', document.body, { metaKey: true, ctrlKey: true }), run)).toBe(false);
  });

  it('matches the physical key: the macOS Korean layout types ₩ on ` and \\', () => {
    mac();
    expect(matchesShortcut(keydown('₩', document.body, { code: 'Backquote', ctrlKey: true }), backquote)).toBe(true);
    expect(matchesShortcut(keydown('₩', document.body, { code: 'Backslash', metaKey: true }), inspector)).toBe(true);
    // Physical Ctrl is required for `ctrl` bindings even on macOS (Cmd is not Ctrl there).
    expect(matchesShortcut(keydown('`', document.body, { code: 'Backquote', metaKey: true }), backquote)).toBe(false);
  });

  it('`ctrl` also satisfies `mod` off macOS', () => {
    win();
    expect(matchesShortcut(keydown('`', document.body, { code: 'Backquote', ctrlKey: true }), backquote)).toBe(true);
    expect(matchesShortcut(keydown('\\', document.body, { code: 'Backslash', ctrlKey: true }), inspector)).toBe(true);
  });

  it('ignores keys typed while an IME composes, except Escape', () => {
    win();
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true, isComposing: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Process', document.body, { keyCode: 229, ctrlKey: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Escape', document.body, { isComposing: true }), esc)).toBe(true);
  });

  it('matches shift and alt exactly', () => {
    win();
    const check: ShortcutBinding = { key: 'Enter', mod: true, shift: true, handler: noop };
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true, shiftKey: true }), check)).toBe(true);
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true }), check)).toBe(false);
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true, shiftKey: true }), run)).toBe(false);
  });

  it('skips plain keys typed into inputs unless the binding opts in', () => {
    const input = document.createElement('textarea');
    document.body.appendChild(input);
    expect(matchesShortcut(keydown('x', input), plain)).toBe(false);
    expect(matchesShortcut(keydown('x', document.body), plain)).toBe(true);
    expect(matchesShortcut(keydown('Escape', input), esc)).toBe(true);
  });

  it('never fires for keys pressed inside an open dialog', () => {
    win();
    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    const field = document.createElement('input');
    dialog.appendChild(field);
    document.body.appendChild(dialog);
    expect(matchesShortcut(keydown('Enter', field, { ctrlKey: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Escape', field), esc)).toBe(false);
    expect(matchesShortcut(keydown('Enter', field, { ctrlKey: true }), { ...run, inDialogs: true })).toBe(true);
  });

  it('checks the extra `when` condition last', () => {
    expect(matchesShortcut(keydown('x', document.body), { ...plain, when: () => false })).toBe(false);
  });
});

describe('editableHasText', () => {
  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('reads input values and contenteditable text, ignoring aria-hidden widgets (CodeMirror placeholder)', () => {
    const input = document.createElement('input');
    expect(editableHasText(input)).toBe(false);
    input.value = 'x';
    expect(editableHasText(input)).toBe(true);

    const content = document.createElement('div');
    // jsdom has no isContentEditable.
    Object.defineProperty(content, 'isContentEditable', { value: true });
    content.innerHTML = '<div class="cm-line"><span class="cm-placeholder" aria-hidden="true">SELECT …</span><br></div>';
    document.body.appendChild(content);
    expect(content.textContent).toBe('SELECT …');
    expect(editableHasText(content)).toBe(false);
    content.innerHTML = '<div class="cm-line">SELECT 1</div>';
    expect(editableHasText(content)).toBe(true);
  });
});

describe('WebView accelerator guard', () => {
  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  const press = (target: EventTarget, init: KeyboardEventInit) => {
    const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
    target.dispatchEvent(e);
    return e;
  };

  it('knows the reload / print keys of each platform', () => {
    win();
    const k = (init: KeyboardEventInit) => isWebViewAccelerator(new KeyboardEvent('keydown', init));
    expect(k({ key: 'F5', code: 'F5' })).toBe(true);
    expect(k({ key: 'F5', code: 'F5', ctrlKey: true })).toBe(true);
    expect(k({ key: 'r', code: 'KeyR', ctrlKey: true })).toBe(true);
    expect(k({ key: 'R', code: 'KeyR', ctrlKey: true, shiftKey: true })).toBe(true);
    expect(k({ key: 'p', code: 'KeyP', ctrlKey: true })).toBe(true);
    expect(k({ key: 'r', code: 'KeyR' })).toBe(false);
    expect(k({ key: 'r', code: 'KeyR', ctrlKey: true, altKey: true })).toBe(false);
    expect(k({ key: 'c', code: 'KeyC', ctrlKey: true })).toBe(false);
    mac();
    expect(k({ key: 'r', code: 'KeyR', metaKey: true })).toBe(true);
    expect(k({ key: 'r', code: 'KeyR', ctrlKey: true })).toBe(false);
  });

  it('prevents them inside a dialog, where no action runs; outside, the dispatcher still runs its action', () => {
    win();
    const refresh = vi.fn();
    const palette = vi.fn();
    renderHook(() => {
      useWebViewAcceleratorGuard();
      useGlobalShortcuts([
        { key: 'F5', code: 'F5', inInputs: true, handler: refresh },
        { key: 'r', code: 'KeyR', mod: true, inInputs: true, handler: refresh },
        { key: 'p', code: 'KeyP', mod: true, inInputs: true, handler: palette },
      ]);
    });
    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    const field = document.createElement('input');
    dialog.appendChild(field);
    document.body.appendChild(dialog);

    expect(press(field, { key: 'F5', code: 'F5' }).defaultPrevented).toBe(true);
    expect(press(field, { key: 'r', code: 'KeyR', ctrlKey: true }).defaultPrevented).toBe(true);
    expect(press(field, { key: 'R', code: 'KeyR', ctrlKey: true, shiftKey: true }).defaultPrevented).toBe(true);
    expect(press(field, { key: 'p', code: 'KeyP', ctrlKey: true }).defaultPrevented).toBe(true);
    expect(refresh).not.toHaveBeenCalled();
    expect(palette).not.toHaveBeenCalled();
    // While an IME composes the dispatcher ignores F5, the guard does not.
    expect(press(document.body, { key: 'F5', code: 'F5', isComposing: true }).defaultPrevented).toBe(true);
    expect(refresh).not.toHaveBeenCalled();

    // Outside a dialog: the guard's preventDefault does not count as "consumed".
    expect(press(document.body, { key: 'F5', code: 'F5' }).defaultPrevented).toBe(true);
    expect(press(document.body, { key: 'p', code: 'KeyP', ctrlKey: true }).defaultPrevented).toBe(true);
    expect(refresh).toHaveBeenCalledTimes(1);
    expect(palette).toHaveBeenCalledTimes(1);
    // Other keys are untouched.
    expect(press(field, { key: 'c', code: 'KeyC', ctrlKey: true }).defaultPrevented).toBe(false);
  });

  it('leaves Cmd combos to the native menu when it owns them (Tauri on macOS)', () => {
    mac();
    renderHook(() => useWebViewAcceleratorGuard(() => true));
    expect(press(document.body, { key: 'r', code: 'KeyR', metaKey: true }).defaultPrevented).toBe(false);
    expect(press(document.body, { key: 'p', code: 'KeyP', metaKey: true }).defaultPrevented).toBe(false);
    expect(press(document.body, { key: 'F5', code: 'F5' }).defaultPrevented).toBe(true);
  });

  it('a component that consumed a key first still keeps it from the dispatcher', () => {
    win();
    const run = vi.fn();
    renderHook(() => useGlobalShortcuts([{ key: 'Enter', mod: true, inInputs: true, handler: run }]));
    const field = document.createElement('input');
    field.addEventListener('keydown', (e) => e.preventDefault());
    document.body.appendChild(field);
    press(field, { key: 'Enter', ctrlKey: true });
    expect(run).not.toHaveBeenCalled();
  });
});
