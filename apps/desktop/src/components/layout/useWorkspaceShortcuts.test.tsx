import type { JobSummary } from 'ipc-types';
import { act, fireEvent, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { SqlEditor } from '@/components/query/SqlEditor';
import { registerBuiltinActions } from '@/lib/actions';
import { inTauri } from '@/lib/ipc';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { editableHasText, isEditable } from '@/lib/shortcuts';
import { SearchField } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore, type AppMode } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { escapeAllowed, useWorkspaceShortcuts, workspaceBindings } from './useWorkspaceShortcuts';

vi.mock('@/lib/ipc', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/ipc')>();
  return { ...actual, inTauri: vi.fn(() => false) };
});

function Harness() {
  const mode = useWorkspaceStore((s) => s.mode) as AppMode;
  const connId = useWorkspaceStore((s) => s.openConnId);
  const [q, setQ] = useState('orders');
  useWorkspaceShortcuts(mode, mode === 'workspace' ? connId : null);
  return <SearchField aria-label="find" value={q} onChange={setQ} />;
}

const running: JobSummary = { id: 'job-1', kind: 'export', state: 'running', connId: 'c-prod', connName: 'prod', label: 'orders', startedAt: '2026-10-27T00:00:00Z' };

describe('useWorkspaceShortcuts (keymap dispatcher)', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
    registerBuiltinActions();
  });
  afterEach(() => vi.restoreAllMocks());

  it('F5 on Welcome is consumed (no WebView reload) and reloads the connections', async () => {
    render(<Harness />);
    const e = new KeyboardEvent('keydown', { key: 'F5', code: 'F5', bubbles: true, cancelable: true });
    act(() => {
      window.dispatchEvent(e);
    });
    expect(e.defaultPrevented).toBe(true);
    await vi.waitFor(() => expect(mockBackend.calls).toContain('conn_list'));
  });

  it('Esc in a search field with text clears it and leaves the cancel confirmation alone; on the page it asks', async () => {
    const user = userEvent.setup();
    useJobsStore.getState().upsertSummary(running);
    render(<Harness />);
    await user.click(screen.getByRole('searchbox'));
    await user.keyboard('{Escape}');
    expect(screen.getByRole('searchbox')).toHaveValue('');
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    // Empty field: Esc reaches the dispatcher.
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().cancelConfirmJobId).toBe('job-1');
    await user.keyboard('{Escape}');
    expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
  });

  describe('Esc from the SQL editor (CodeMirror contenteditable)', () => {
    // jsdom has no `isContentEditable`: without it `.cm-content` would not even count as editable
    // (which hid this bug). It also lacks Range rects, which CodeMirror's measuring asks for.
    const proto = HTMLElement.prototype;
    const had = Object.getOwnPropertyDescriptor(proto, 'isContentEditable');
    const rangeProto = Range.prototype as Partial<Range>;
    const hadRects = 'getClientRects' in Range.prototype;
    beforeAll(() => {
      Object.defineProperty(proto, 'isContentEditable', {
        configurable: true,
        get(this: HTMLElement) {
          const host = this.closest('[contenteditable]');
          return !!host && host.getAttribute('contenteditable') !== 'false';
        },
      });
      if (!hadRects) {
        rangeProto.getClientRects = () => Object.assign([], { item: () => null }) as unknown as DOMRectList;
        rangeProto.getBoundingClientRect = () =>
          ({ x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: () => ({}) }) as DOMRect;
      }
    });
    afterAll(() => {
      if (had) Object.defineProperty(proto, 'isContentEditable', had);
      else Reflect.deleteProperty(proto, 'isContentEditable');
      if (!hadRects) {
        delete rangeProto.getClientRects;
        delete rangeProto.getBoundingClientRect;
      }
    });

    function EditorHarness({ value }: { value: string }) {
      useWorkspaceShortcuts('welcome', null);
      return <SqlEditor connId="c-prod" value={value} onChange={() => undefined} placeholder="SELECT …" aria-label="SQL" />;
    }

    it('asks to cancel while the editor holds SQL (focus stays there after Ctrl+Enter)', () => {
      useJobsStore.getState().upsertSummary(running);
      render(<EditorHarness value="SELECT * FROM orders" />);
      const content = screen.getByRole('textbox', { name: 'SQL' });
      expect(isEditable(content)).toBe(true);
      expect(editableHasText(content)).toBe(true);
      act(() => {
        fireEvent.keyDown(content, { key: 'Escape', code: 'Escape' });
      });
      expect(useUiStore.getState().cancelConfirmJobId).toBe('job-1');
      // A second Esc dismisses the confirmation.
      act(() => {
        fireEvent.keyDown(content, { key: 'Escape', code: 'Escape' });
      });
      expect(useUiStore.getState().cancelConfirmJobId).toBeNull();
    });

    it('asks from an empty editor too: the placeholder widget is not text', () => {
      useJobsStore.getState().upsertSummary(running);
      const { container } = render(<EditorHarness value="" />);
      const content = screen.getByRole('textbox', { name: 'SQL' });
      expect(container.querySelector('.cm-placeholder')).not.toBeNull();
      expect(content.textContent).not.toBe('');
      expect(editableHasText(content)).toBe(false);
      act(() => {
        fireEvent.keyDown(content, { key: 'Escape', code: 'Escape' });
      });
      expect(useUiStore.getState().cancelConfirmJobId).toBe('job-1');
    });

    it('never from the completion popup', () => {
      const tooltip = document.createElement('div');
      tooltip.className = 'cm-tooltip-autocomplete';
      const editor = document.createElement('div');
      editor.className = 'cm-editor';
      editor.appendChild(tooltip);
      document.body.appendChild(editor);
      const e = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true });
      Object.defineProperty(e, 'target', { value: tooltip });
      expect(escapeAllowed(e)).toBe(false);
      const inEditor = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true });
      Object.defineProperty(inEditor, 'target', { value: editor });
      expect(escapeAllowed(inEditor)).toBe(true);
      editor.remove();
    });
  });

  it('Ctrl+` (₩ on the macOS Korean layout) toggles the export panel only while a query target is active', async () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
    render(<Harness />);
    const press = () =>
      act(() => {
        window.dispatchEvent(new KeyboardEvent('keydown', { key: '₩', code: 'Backquote', ctrlKey: true, bubbles: true, cancelable: true }));
      });
    await act(async () => {
      await useWorkspaceStore.getState().openWorkspace('c-local');
    });
    const tabId = Object.keys(useExtractStore.getState().docs)[0];
    expect(useExtractStore.getState().docs[tabId].panelCollapsed).toBe(true);
    press();
    await vi.waitFor(() => expect(useExtractStore.getState().docs[tabId].panelCollapsed).toBe(false));
    // Mod+\ (₩ too) toggles the inspector.
    act(() => {
      window.dispatchEvent(new KeyboardEvent('keydown', { key: '₩', code: 'Backslash', metaKey: true, bubbles: true, cancelable: true }));
    });
    await vi.waitFor(() => expect(useUiStore.getState().inspectorOpen).toBe(true));
  });

  it('skips menu-owned rows inside Tauri on macOS only; workspace rows only in a workspace', () => {
    const run = vi.fn();
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
    const browser = workspaceBindings('workspace', 'c-prod', run);
    vi.mocked(inTauri).mockReturnValue(true);
    const tauriMac = workspaceBindings('workspace', 'c-prod', run);
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    const tauriWin = workspaceBindings('workspace', 'c-prod', run);
    const welcome = workspaceBindings('welcome', null, run);
    const keys = (bs: typeof browser) => bs.map((b) => `${b.mod ? 'mod+' : ''}${b.shift ? 'shift+' : ''}${b.key.toLowerCase()}`);
    expect(keys(browser)).toContain('mod+w');
    expect(keys(tauriMac)).not.toContain('mod+w');
    expect(keys(tauriMac)).toContain('mod+shift+t');
    expect(keys(tauriWin)).toContain('mod+w');
    expect(keys(welcome)).not.toContain('mod+w');
    expect(keys(welcome)).toContain('f5');
    // Welcome label rows are never bound.
    expect(keys(browser)).not.toContain('mod+f');
  });
});
