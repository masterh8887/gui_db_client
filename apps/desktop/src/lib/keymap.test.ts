import { afterEach, describe, expect, it, vi } from 'vitest';
import { bindingFor, comboLabel, KEYMAP, parseCombo, shortcutFor } from './keymap';

describe('keymap', () => {
  afterEach(() => vi.restoreAllMocks());

  it('labels combos per platform, keeping the key as written', () => {
    expect(comboLabel('mod+shift+enter', true)).toBe('⇧⌘↩');
    expect(comboLabel('mod+shift+enter', false)).toBe('Ctrl+Shift+Enter');
    expect(comboLabel('ctrl+`', true)).toBe('⌃`');
    expect(comboLabel('ctrl+`', false)).toBe('Ctrl+`');
    expect(comboLabel('mod+\\', true)).toBe('⌘\\');
    expect(comboLabel('mod+backspace', true)).toBe('⌘⌫');
    expect(comboLabel('delete', false)).toBe('Delete');
    expect(comboLabel('f5', false)).toBe('F5');
    expect(comboLabel('mod+.', false)).toBe('Ctrl+.');
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
    expect(comboLabel('mod+p')).toBe('⌘P');
  });

  it('parses combos into a ShortcutBinding with the physical code', () => {
    expect(parseCombo('mod+shift+o')).toEqual({ key: 'o', mod: true, ctrl: false, shift: true, alt: false });
    const b = bindingFor({ combo: 'mod+enter', code: 'Enter', action: 'query.run', scope: 'workspace' }, () => undefined);
    expect(b).toMatchObject({ key: 'Enter', code: 'Enter', mod: true, shift: false });
    expect(bindingFor({ combo: 'f5', code: 'F5', action: 'x', scope: 'always' }, () => undefined).key).toBe('F5');
  });

  it('covers the section 6 map', () => {
    const actions = new Set(KEYMAP.map((k) => k.action));
    for (const id of [
      'palette.open',
      'palette.connections',
      'connection.new',
      'app.settings',
      'view.toggleConsole',
      'jobs.cancelAsk',
      'catalog.refresh',
      'tabs.newQuery',
      'tabs.close',
      'tabs.reopenClosed',
      'tabs.select1',
      'tabs.selectLast',
      'tabs.next',
      'tabs.previous',
      'query.run',
      'query.explain',
      'query.check',
      'query.openFile',
      'view.toggleExportPanel',
      'catalog.switchDatabase',
      'sidebar.focusSearch',
      'view.toggleSidebar',
      'view.toggleInspector',
    ]) {
      expect(actions.has(id), id).toBe(true);
    }
    expect(shortcutFor('view.toggleInspector')).toBe('mod+\\');
    expect(KEYMAP.filter((k) => k.action === 'catalog.refresh').map((k) => k.combo)).toEqual(['f5', 'mod+r', 'mod+shift+r']);
  });

  it('Mod+Shift+N opens a new window and is a macOS menu item (docs/db-copy.md 1.1)', () => {
    const row = KEYMAP.find((k) => k.combo === 'mod+shift+n');
    expect(row).toMatchObject({ action: 'window.new', scope: 'always', menuOwned: true });
  });
});
