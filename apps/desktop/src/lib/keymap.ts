// The app keyboard map (DESIGN.md §6). `useWorkspaceShortcuts` (layout) turns every row into a
// `ShortcutBinding` that calls `runAction(action)`; `KeyHint` renders `comboLabel(combo)`.
//
// combo syntax: lowercase parts joined by `+`: modifiers `mod` (Cmd / Ctrl), `ctrl` (physical
// Ctrl), `shift`, `alt`, then one key (`p`, `enter`, `escape`, `f5`, `` ` ``, `\`, `,`, `.`,
// `1`, `tab`, `backspace`, `delete`, `arrowup`, ...).
//
// scope: `always` (both modes), `workspace`, `queryTarget` (a query tab or a table tab in Query
// view is active), `welcome` (label rows only: WelcomeScreen binds its own keys while mounted).
// `menuOwned` rows are also macOS menu items: inside Tauri on macOS the menu fires them (as
// `menu:action`), so the JS dispatcher skips them there and nothing fires twice.

import { isMac } from './platform';
import type { ShortcutBinding } from './shortcuts';

export interface KeyBinding {
  combo: string;
  code?: string;
  action: string;
  scope: 'always' | 'welcome' | 'workspace' | 'queryTarget';
  inInputs?: boolean;
  menuOwned?: boolean;
}

const tabDigits: KeyBinding[] = Array.from({ length: 8 }, (_, i) => ({
  combo: `mod+${i + 1}`,
  code: `Digit${i + 1}`,
  action: `tabs.select${i + 1}`,
  scope: 'workspace' as const,
  inInputs: true,
}));

export const KEYMAP: readonly KeyBinding[] = [
  { combo: 'mod+p', code: 'KeyP', action: 'palette.open', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'mod+shift+o', code: 'KeyO', action: 'palette.connections', scope: 'always', inInputs: true },
  { combo: 'mod+n', code: 'KeyN', action: 'connection.new', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'mod+shift+n', code: 'KeyN', action: 'window.new', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'mod+,', code: 'Comma', action: 'app.settings', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'mod+j', code: 'KeyJ', action: 'view.toggleConsole', scope: 'always', inInputs: true, menuOwned: true },
  // Esc: inputs only while empty (enforced by the dispatcher); Mod+. also inside inputs.
  { combo: 'escape', code: 'Escape', action: 'jobs.cancelAsk', scope: 'always', inInputs: true },
  { combo: 'mod+.', code: 'Period', action: 'jobs.cancelAsk', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'f5', code: 'F5', action: 'catalog.refresh', scope: 'always', inInputs: true },
  { combo: 'mod+r', code: 'KeyR', action: 'catalog.refresh', scope: 'always', inInputs: true, menuOwned: true },
  { combo: 'mod+shift+r', code: 'KeyR', action: 'catalog.refresh', scope: 'always', inInputs: true },

  // Welcome (labels only; WelcomeScreen binds these with useGlobalShortcuts while mounted).
  { combo: 'mod+f', code: 'KeyF', action: 'welcome.search', scope: 'welcome' },
  { combo: 'enter', code: 'Enter', action: 'welcome.open', scope: 'welcome' },
  { combo: 'delete', code: 'Delete', action: 'welcome.delete', scope: 'welcome' },
  { combo: 'mod+backspace', code: 'Backspace', action: 'welcome.delete', scope: 'welcome' },

  { combo: 'mod+t', code: 'KeyT', action: 'tabs.newQuery', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+w', code: 'KeyW', action: 'tabs.close', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+shift+t', code: 'KeyT', action: 'tabs.reopenClosed', scope: 'workspace', inInputs: true },
  ...tabDigits,
  { combo: 'mod+9', code: 'Digit9', action: 'tabs.selectLast', scope: 'workspace', inInputs: true },
  { combo: 'ctrl+tab', code: 'Tab', action: 'tabs.next', scope: 'workspace', inInputs: true },
  { combo: 'ctrl+shift+tab', code: 'Tab', action: 'tabs.previous', scope: 'workspace', inInputs: true },
  { combo: 'mod+enter', code: 'Enter', action: 'query.run', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+e', code: 'KeyE', action: 'query.explain', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+shift+enter', code: 'Enter', action: 'query.check', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+o', code: 'KeyO', action: 'query.openFile', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'ctrl+`', code: 'Backquote', action: 'view.toggleExportPanel', scope: 'queryTarget', inInputs: true },
  { combo: 'mod+k', code: 'KeyK', action: 'catalog.switchDatabase', scope: 'workspace', inInputs: true },
  { combo: 'mod+shift+f', code: 'KeyF', action: 'sidebar.focusSearch', scope: 'workspace', inInputs: true },
  { combo: 'mod+0', code: 'Digit0', action: 'view.toggleSidebar', scope: 'workspace', inInputs: true, menuOwned: true },
  { combo: 'mod+\\', code: 'Backslash', action: 'view.toggleInspector', scope: 'workspace', inInputs: true, menuOwned: true },
];

/** The first combo bound to `action` (for KeyHints next to buttons and menu items). */
export function shortcutFor(action: string): string | undefined {
  return KEYMAP.find((b) => b.action === action)?.combo;
}

export interface ParsedCombo {
  key: string;
  mod: boolean;
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
}

export function parseCombo(combo: string): ParsedCombo {
  // `+` itself can be the key (`mod++`): split on `+` that is followed by something.
  const parts = combo.toLowerCase().split(/\+(?=.)/);
  const out: ParsedCombo = { key: '', mod: false, ctrl: false, shift: false, alt: false };
  for (const p of parts) {
    if (p === 'mod' || p === 'cmd' || p === 'meta') out.mod = true;
    else if (p === 'ctrl' || p === 'control') out.ctrl = true;
    else if (p === 'shift') out.shift = true;
    else if (p === 'alt' || p === 'option') out.alt = true;
    else out.key = p;
  }
  return out;
}

/** `KeyboardEvent.key` value for a combo key. */
function eventKey(key: string): string {
  switch (key) {
    case 'enter':
      return 'Enter';
    case 'escape':
    case 'esc':
      return 'Escape';
    case 'tab':
      return 'Tab';
    case 'backspace':
      return 'Backspace';
    case 'delete':
      return 'Delete';
    case 'space':
      return ' ';
    case 'arrowup':
    case 'up':
      return 'ArrowUp';
    case 'arrowdown':
    case 'down':
      return 'ArrowDown';
    case 'arrowleft':
    case 'left':
      return 'ArrowLeft';
    case 'arrowright':
    case 'right':
      return 'ArrowRight';
    case 'home':
      return 'Home';
    case 'end':
      return 'End';
    default:
      return /^f\d{1,2}$/.test(key) ? key.toUpperCase() : key;
  }
}

/** A `ShortcutBinding` for one KEYMAP row (the handler is the caller's). */
export function bindingFor(b: KeyBinding, handler: (e: KeyboardEvent) => void): ShortcutBinding {
  const c = parseCombo(b.combo);
  return { key: eventKey(c.key), code: b.code, mod: c.mod, ctrl: c.ctrl, shift: c.shift, alt: c.alt, inInputs: b.inInputs, handler };
}

const MAC_SYMBOLS: Record<string, string> = {
  enter: '↩',
  escape: 'Esc',
  esc: 'Esc',
  tab: '⇥',
  backspace: '⌫',
  delete: '⌦',
  space: 'Space',
  arrowup: '↑',
  up: '↑',
  arrowdown: '↓',
  down: '↓',
  arrowleft: '←',
  left: '←',
  arrowright: '→',
  right: '→',
  home: 'Home',
  end: 'End',
};

const WIN_NAMES: Record<string, string> = {
  enter: 'Enter',
  escape: 'Esc',
  esc: 'Esc',
  tab: 'Tab',
  backspace: 'Backspace',
  delete: 'Delete',
  space: 'Space',
  arrowup: '↑',
  up: '↑',
  arrowdown: '↓',
  down: '↓',
  arrowleft: '←',
  left: '←',
  arrowright: '→',
  right: '→',
  home: 'Home',
  end: 'End',
};

function keyLabel(key: string, mac: boolean): string {
  const table = mac ? MAC_SYMBOLS : WIN_NAMES;
  if (table[key]) return table[key];
  if (/^f\d{1,2}$/.test(key)) return key.toUpperCase();
  return key.length === 1 ? key.toUpperCase() : key.charAt(0).toUpperCase() + key.slice(1);
}

/**
 * Display label: macOS `⌘⇧↩`, `⌃\``, `⌘\`; Windows `Ctrl+Shift+Enter`, `Ctrl+\``, `F5`. The
 * key is shown as written (a `\` never becomes `₩`).
 */
export function comboLabel(combo: string, mac: boolean = isMac()): string {
  const c = parseCombo(combo);
  const key = keyLabel(c.key, mac);
  if (mac) {
    return `${c.ctrl ? '⌃' : ''}${c.alt ? '⌥' : ''}${c.shift ? '⇧' : ''}${c.mod ? '⌘' : ''}${key}`;
  }
  const parts: string[] = [];
  if (c.mod || c.ctrl) parts.push('Ctrl');
  if (c.alt) parts.push('Alt');
  if (c.shift) parts.push('Shift');
  parts.push(key);
  return parts.join('+');
}
