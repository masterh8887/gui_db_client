import { afterEach, describe, expect, it, vi } from 'vitest';
import { matchesShortcut, type ShortcutBinding } from './shortcuts';

const noop = () => undefined;
const run: ShortcutBinding = { key: 'Enter', mod: true, handler: noop };
const esc: ShortcutBinding = { key: 'Escape', inInputs: true, handler: noop };
const plain: ShortcutBinding = { key: 'x', handler: noop };

function keydown(key: string, target: Element, init: KeyboardEventInit = {}): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { key, bubbles: true, ...init });
  Object.defineProperty(e, 'target', { value: target });
  return e;
}

describe('matchesShortcut', () => {
  afterEach(() => {
    document.body.innerHTML = '';
    vi.restoreAllMocks();
  });

  it('requires the platform modifier (Ctrl off macOS)', () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    const body = document.body;
    expect(matchesShortcut(keydown('Enter', body, { ctrlKey: true }), run)).toBe(true);
    expect(matchesShortcut(keydown('Enter', body, { metaKey: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Enter', body), run)).toBe(false);
    expect(matchesShortcut(keydown('Enter', body, { ctrlKey: true, altKey: true }), run)).toBe(false);
  });

  it('uses Cmd on macOS', () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
    expect(matchesShortcut(keydown('Enter', document.body, { metaKey: true }), run)).toBe(true);
    expect(matchesShortcut(keydown('Enter', document.body, { ctrlKey: true }), run)).toBe(false);
  });

  it('skips plain keys typed into inputs unless the binding opts in', () => {
    const input = document.createElement('textarea');
    document.body.appendChild(input);
    expect(matchesShortcut(keydown('x', input), plain)).toBe(false);
    expect(matchesShortcut(keydown('x', document.body), plain)).toBe(true);
    expect(matchesShortcut(keydown('Escape', input), esc)).toBe(true);
  });

  it('never fires for keys pressed inside an open dialog', () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    const field = document.createElement('input');
    dialog.appendChild(field);
    document.body.appendChild(dialog);
    expect(matchesShortcut(keydown('Enter', field, { ctrlKey: true }), run)).toBe(false);
    expect(matchesShortcut(keydown('Escape', field), esc)).toBe(false);
    expect(matchesShortcut(keydown('Enter', field, { ctrlKey: true }), { ...run, inDialogs: true })).toBe(true);
  });
});
