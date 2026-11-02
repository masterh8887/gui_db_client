'use client';

// Toasts (DESIGN.md §3.3 "Feedback"): only for background outcomes (rescan count, import,
// duplicate, metadata reload). Copies and inline results never toast. `Toaster`
// (components/ui, mounted once by the AppShell) renders the stack.

import { create } from 'zustand';

export type ToastTone = 'neutral' | 'success' | 'danger';

export interface ToastItem {
  id: number;
  message: string;
  tone: ToastTone;
}

interface ToastState {
  toasts: ToastItem[];
}

export const useToastStore = create<ToastState>()(() => ({ toasts: [] }));

const MAX_TOASTS = 4;
const timers = new Map<number, ReturnType<typeof setTimeout>>();
let nextId = 1;

export function dismissToast(id: number): void {
  const t = timers.get(id);
  if (t) clearTimeout(t);
  timers.delete(id);
  useToastStore.setState((s) => ({ toasts: s.toasts.filter((x) => x.id !== id) }));
}

export function toast(message: string, o?: { tone?: ToastTone; durationMs?: number }): void {
  const tone = o?.tone ?? 'neutral';
  const id = nextId++;
  useToastStore.setState((s) => ({ toasts: [...s.toasts, { id, message, tone }].slice(-MAX_TOASTS) }));
  const ms = o?.durationMs ?? (tone === 'danger' ? 6000 : 3500);
  if (ms > 0) timers.set(id, setTimeout(() => dismissToast(id), ms));
}

export function __resetForTests(): void {
  for (const t of timers.values()) clearTimeout(t);
  timers.clear();
  useToastStore.setState({ toasts: [] });
}
