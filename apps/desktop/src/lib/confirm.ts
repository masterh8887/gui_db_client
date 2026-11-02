'use client';

// In-app confirmation (DESIGN.md §5.I "Confirm"): replaces every `window.confirm`. `askConfirm`
// queues a request that `ConfirmHost` (components/ui, mounted once by the AppShell) shows as a
// small sheet; Esc, scrim and Cancel resolve `false`. Tests install a handler with
// `setConfirmHandler(async () => true)` instead of rendering the host.

import { create } from 'zustand';

export interface ConfirmOptions {
  title: string;
  body?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Red confirm button (destructive action). */
  danger?: boolean;
}

export interface ConfirmRequest {
  id: number;
  options: ConfirmOptions;
  resolve: (ok: boolean) => void;
}

interface ConfirmQueueState {
  queue: ConfirmRequest[];
  /** Number of mounted `ConfirmHost`s; without one (and no handler) requests resolve `false`. */
  hosts: number;
}

export const useConfirmQueue = create<ConfirmQueueState>()(() => ({ queue: [], hosts: 0 }));

let handler: ((o: ConfirmOptions) => Promise<boolean>) | null = null;
let nextId = 1;

/** Ask the user; resolves `true` only for an explicit confirm. */
export function askConfirm(o: ConfirmOptions): Promise<boolean> {
  if (handler) return handler(o);
  if (useConfirmQueue.getState().hosts === 0) return Promise.resolve(false);
  return new Promise<boolean>((resolve) => {
    const req: ConfirmRequest = { id: nextId++, options: o, resolve };
    useConfirmQueue.setState((s) => ({ queue: [...s.queue, req] }));
  });
}

/** Answer the request `id` (ConfirmHost) and drop it from the queue. */
export function answerConfirm(id: number, ok: boolean): void {
  const req = useConfirmQueue.getState().queue.find((r) => r.id === id);
  if (!req) return;
  useConfirmQueue.setState((s) => ({ queue: s.queue.filter((r) => r.id !== id) }));
  req.resolve(ok);
}

/** Tests: answer every confirm through `fn`; `null` restores the ConfirmHost. */
export function setConfirmHandler(fn: ((o: ConfirmOptions) => Promise<boolean>) | null): void {
  handler = fn;
}

/** ConfirmHost registration (mount / unmount). */
export function registerConfirmHost(): () => void {
  useConfirmQueue.setState((s) => ({ hosts: s.hosts + 1 }));
  return () => useConfirmQueue.setState((s) => ({ hosts: Math.max(0, s.hosts - 1) }));
}

export function __resetForTests(): void {
  for (const r of useConfirmQueue.getState().queue) r.resolve(false);
  useConfirmQueue.setState({ queue: [] });
  handler = null;
}
