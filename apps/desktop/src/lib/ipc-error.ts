import type { ErrorPayload } from 'ipc-types';

/**
 * Error thrown by every IPC call. Mirrors `ErrorPayload { code, message, detail? }` from core:
 * `code` is one of the `CoreError::code()` values (`CONNECT, AUTH, QUERY, IO, CANCELLED, TIMEOUT,
 * DRIVER_MISSING, OOM_RISK, INVALID, TUNNEL, NOT_FOUND, INTERNAL`).
 */
export class IpcError extends Error {
  code: string;
  detail?: string;
  constructor(p: ErrorPayload) {
    super(p.message);
    this.name = 'IpcError';
    this.code = p.code;
    this.detail = p.detail ?? undefined;
  }

  toPayload(): ErrorPayload {
    return { code: this.code, message: this.message, detail: this.detail };
  }
}

export function isIpcError(e: unknown): e is IpcError {
  return e instanceof IpcError;
}

/** Turn anything thrown into an `IpcError` (never loses the code when one is present). */
export function toIpcError(e: unknown): IpcError {
  if (isIpcError(e)) return e;
  if (e && typeof e === 'object' && 'code' in e && 'message' in e) {
    const p = e as { code: unknown; message: unknown; detail?: unknown };
    return new IpcError({
      code: String(p.code),
      message: String(p.message),
      detail: p.detail == null ? undefined : String(p.detail),
    });
  }
  if (e instanceof Error) return new IpcError({ code: 'INTERNAL', message: e.message });
  return new IpcError({ code: 'INTERNAL', message: String(e) });
}

/** Human readable one-liner for toasts and log lines. */
export function errorText(e: unknown): string {
  const err = toIpcError(e);
  return err.detail ? `${err.code}: ${err.message} (${err.detail})` : `${err.code}: ${err.message}`;
}
