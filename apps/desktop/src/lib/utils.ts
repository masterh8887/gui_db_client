import { clsx, type ClassValue } from 'clsx';

/** Class name joiner (clsx). Tailwind utilities are plain strings, so no merge step is needed. */
export function cn(...inputs: ClassValue[]): string {
  return clsx(inputs);
}

/** Copy text to the clipboard; falls back to a hidden textarea when the async API is missing. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the legacy path
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/** Stable key for a table reference inside one connection. */
export function tableKey(connId: string, ref: { database?: string; schema?: string; name: string }): string {
  return `${connId}/${ref.database ?? ''}/${ref.schema ?? ''}/${ref.name}`;
}

/** `schema.name` or `name` for display. */
export function tableLabel(ref: { schema?: string; name: string }): string {
  return ref.schema ? `${ref.schema}.${ref.name}` : ref.name;
}

