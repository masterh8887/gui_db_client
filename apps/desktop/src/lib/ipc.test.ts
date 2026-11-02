// `listenWindow` (docs/db-copy.md 1.1): window-targeted events are listened to with this window's
// WebviewWindow target, so an `emit_to` for another window never reaches this one.

import { describe, expect, it, vi } from 'vitest';

describe('listenWindow', () => {
  it('listens only to this window (WebviewWindow target)', async () => {
    vi.resetModules();
    vi.doMock('@tauri-apps/api/core', () => ({ invoke: vi.fn(), isTauri: () => true }));
    vi.doMock('@tauri-apps/api/webviewWindow', () => ({ getCurrentWebviewWindow: () => ({ label: 'w-3' }) }));
    try {
      const { listenWindow } = await import('./ipc');
      const { listen } = await import('@tauri-apps/api/event');
      await listenWindow('tunnel:hostkey', () => undefined);
      expect(listen).toHaveBeenCalledWith('tunnel:hostkey', expect.any(Function), { target: { kind: 'WebviewWindow', label: 'w-3' } });
    } finally {
      vi.doUnmock('@tauri-apps/api/core');
      vi.doUnmock('@tauri-apps/api/webviewWindow');
    }
  });
});
