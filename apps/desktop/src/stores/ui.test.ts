import { beforeEach, describe, expect, it } from 'vitest';
import { safeLocalStorage } from '@/lib/storage';
import { resetStores } from '@/test/stores';
import { LAYOUT_DEFAULTS, LAYOUT_LIMITS, migrateUi, rehydrateUi, useUiStore } from './ui';

const ui = () => useUiStore.getState();

describe('ui store', () => {
  beforeEach(() => {
    resetStores();
    safeLocalStorage().removeItem('bulkdb-ui');
  });

  it('has the redesign defaults: inspector closed, sidebar open on Items', () => {
    expect(ui()).toMatchObject({
      inspectorOpen: false,
      sidebarOpen: true,
      sidebarSegment: 'items',
      inspectorSegment: 'activity',
      leftWidth: LAYOUT_DEFAULTS.leftWidth,
      rightWidth: LAYOUT_DEFAULTS.rightWidth,
      logHeight: LAYOUT_DEFAULTS.logHeight,
      exportPanelHeight: LAYOUT_DEFAULTS.exportPanelHeight,
      welcomeGroupBy: 'none',
    });
  });

  it('merges password requests for the same connection and queues other connections oldest first', () => {
    const then = () => undefined;
    ui().askDbPassword('a', 'open');
    ui().askDbPassword('b', 'browse');
    ui().askDbPassword('a', 'run', { tabId: 'q1', then });
    const prompts = ui().dbPasswordPrompts;
    expect(prompts.map((p) => p.connId)).toEqual(['a', 'b']);
    expect(prompts[0].requests).toEqual([
      { reason: 'open', tabId: undefined, then: undefined },
      { reason: 'run', tabId: 'q1', then },
    ]);
    expect(prompts[0].id).not.toBe(prompts[1].id);
    // Legacy mirror of the head prompt.
    expect(ui().dbPasswordPrompt).toEqual({ connId: 'a', reason: 'open' });
    ui().resolveDbPassword(prompts[0].id);
    expect(ui().dbPasswordPrompts.map((p) => p.connId)).toEqual(['b']);
    expect(ui().dbPasswordPrompt).toEqual({ connId: 'b', reason: 'browse' });
    ui().closeDbPassword();
    expect(ui().dbPasswordPrompts).toEqual([]);
    expect(ui().dbPasswordPrompt).toBeNull();
  });

  it('requestSidebar opens the sidebar on Items with a fresh nonce', () => {
    ui().setSidebarOpen(false);
    ui().setSidebarSegment('history');
    ui().requestSidebar('database');
    const first = ui().sidebarRequest;
    expect(first?.kind).toBe('database');
    expect(ui()).toMatchObject({ sidebarOpen: true, sidebarSegment: 'items' });
    ui().requestSidebar('database');
    expect(ui().sidebarRequest?.nonce).toBe((first?.nonce ?? 0) + 1);
    ui().clearSidebarRequest();
    expect(ui().sidebarRequest).toBeNull();
  });

  it('opens and closes the palette with a scope, a query and a pick', () => {
    ui().openPalette('connections', '@', { kind: 'openInConnection', tabId: 'q1' });
    expect(ui().palette).toEqual({ open: true, scope: 'connections', query: '@', pick: { kind: 'openInConnection', tabId: 'q1' } });
    ui().closePalette();
    expect(ui().palette).toEqual({ open: false, scope: 'all', query: '' });
  });

  it('toggles collapsed sections and clamps sizes', () => {
    ui().toggleSection('items:view');
    expect(ui().collapsedSections).toEqual({ 'items:view': true });
    ui().toggleSection('items:view');
    expect(ui().collapsedSections).toEqual({});
    ui().setLeftWidth(10);
    expect(ui().leftWidth).toBe(LAYOUT_LIMITS.leftMin);
    ui().setExportPanelHeight(5000);
    expect(ui().exportPanelHeight).toBe(LAYOUT_LIMITS.exportMax);
  });

  it('migrates and clamps an older persisted state (version 0)', async () => {
    expect(migrateUi({ leftWidth: 900, rightWidth: 'x', logHeight: 10, theme: 'dark', lang: 'en', inspectorOpen: true, collapsedSections: { a: true, b: 1 } })).toEqual({
      leftWidth: LAYOUT_LIMITS.leftMax,
      rightWidth: LAYOUT_DEFAULTS.rightWidth,
      logHeight: LAYOUT_LIMITS.logMin,
      exportPanelHeight: LAYOUT_DEFAULTS.exportPanelHeight,
      theme: 'dark',
      lang: 'en',
      inspectorOpen: true,
      collapsedSections: { a: true },
    });
    safeLocalStorage().setItem('bulkdb-ui', JSON.stringify({ state: { leftWidth: 1000, logHeight: 50, theme: 'light' }, version: 0 }));
    await rehydrateUi();
    expect(ui()).toMatchObject({ leftWidth: LAYOUT_LIMITS.leftMax, logHeight: LAYOUT_LIMITS.logMin, theme: 'light', inspectorOpen: false });
  });

  it('persists the layout fields only', () => {
    ui().setInspectorOpen(true);
    ui().setWelcomeGroupBy('tag');
    ui().openPalette();
    const saved = JSON.parse(safeLocalStorage().getItem('bulkdb-ui') ?? '{}') as { state: Record<string, unknown>; version: number };
    expect(saved.version).toBe(1);
    expect(saved.state).toMatchObject({ inspectorOpen: true, welcomeGroupBy: 'tag' });
    expect(saved.state.palette).toBeUndefined();
    expect(saved.state.dbPasswordPrompts).toBeUndefined();
  });
});
