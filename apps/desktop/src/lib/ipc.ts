// Typed wrappers over Tauri `invoke` / `listen`. Payload types come from `ipc-types`
// (generated from Rust by ts-rs); nothing here is hand-typed beyond the command names.
//
// Outside the Tauri WebView (plain `next dev`, vitest) every call is routed to the in-browser
// fake backend in `./mock-ipc` so the UI can be developed, demoed and tested without Rust. The
// mock is loaded with a dynamic `import()`, so inside Tauri its code is never fetched.
//
// Numbers: ts-rs renders Rust `u64`/`i64` as `bigint`, but serde writes them as plain JSON
// numbers, so at runtime incoming payloads carry `number`s. Consumers go through `num()` in
// `./format`. Outgoing arguments are normalised here: any `bigint` becomes a `number` before
// `JSON.stringify` (which cannot serialise bigint) sees it.

import { invoke as tauriInvoke, isTauri } from '@tauri-apps/api/core';
import { listen as tauriListen, type UnlistenFn } from '@tauri-apps/api/event';
import { toIpcError } from './ipc-error';

type MockModule = typeof import('./mock-ipc');
let mockModule: Promise<MockModule> | null = null;

/** The in-browser fake backend, loaded on first use. */
function mock(): Promise<MockModule> {
  mockModule ??= import('./mock-ipc');
  return mockModule;
}

export { IpcError, isIpcError, toIpcError, errorText } from './ipc-error';

export function inTauri(): boolean {
  try {
    return isTauri();
  } catch {
    return false;
  }
}

/** Deep-copy `v` replacing every `bigint` with a `number` (JSON cannot carry bigint). */
export function plainArgs<T>(v: T): T {
  if (typeof v === 'bigint') return Number(v) as unknown as T;
  if (Array.isArray(v)) return v.map(plainArgs) as unknown as T;
  if (v && typeof v === 'object') {
    const out: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
      if (val !== undefined) out[k] = plainArgs(val);
    }
    return out as T;
  }
  return v;
}

/** Call a command; errors arrive as `{ code, message, detail? }` and are rethrown as `IpcError`. */
export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const a = args ? plainArgs(args) : undefined;
  if (!inTauri()) {
    try {
      return await (await mock()).mockInvoke<T>(cmd, a);
    } catch (e) {
      throw toIpcError(e);
    }
  }
  try {
    return await tauriInvoke<T>(cmd, a);
  } catch (e) {
    throw toIpcError(e);
  }
}

export async function listen<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  if (!inTauri()) return (await mock()).mockListen<T>(event, handler);
  return tauriListen<T>(event, (e) => handler(e.payload));
}
