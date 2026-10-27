import '@testing-library/jest-dom/vitest';
import { afterEach, vi } from 'vitest';
import { mockBackend } from '@/lib/mock-ipc';

// The Tauri runtime is not present under vitest: `isTauri()` is false, so `@/lib/ipc` routes
// every call to the in-browser mock backend (`@/lib/mock-ipc`).
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async () => undefined),
  isTauri: () => false,
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => undefined),
}));

// jsdom lacks a few DOM APIs that Radix primitives touch.
if (typeof window !== 'undefined') {
  if (!('ResizeObserver' in window)) {
    class ResizeObserverStub {
      observe() {}
      unobserve() {}
      disconnect() {}
    }
    Object.defineProperty(window, 'ResizeObserver', { value: ResizeObserverStub, writable: true });
  }
  const proto = window.Element.prototype as Element & { hasPointerCapture?: unknown; setPointerCapture?: unknown; releasePointerCapture?: unknown; scrollIntoView?: unknown };
  if (!proto.hasPointerCapture) proto.hasPointerCapture = () => false;
  if (!proto.setPointerCapture) proto.setPointerCapture = () => undefined;
  if (!proto.releasePointerCapture) proto.releasePointerCapture = () => undefined;
  if (!proto.scrollIntoView) proto.scrollIntoView = () => undefined;
}

afterEach(() => {
  // Clear mock timers / state between tests so a running fake job never leaks.
  mockBackend.reset();
});
