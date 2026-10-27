// Native file / folder pickers. Inside Tauri they go through `@tauri-apps/plugin-dialog`
// (loaded lazily so the browser build never touches it); outside, a plain prompt stands in.

import { logLocal } from '@/stores/logs';
import { openPath } from './commands';
import { inTauri } from './ipc';
import { errorText } from './ipc-error';

export async function pickSqlFile(): Promise<string | null> {
  if (inTauri()) {
    const { open } = await import('@tauri-apps/plugin-dialog');
    const picked = await open({ multiple: false, directory: false, filters: [{ name: 'SQL', extensions: ['sql'] }] });
    return typeof picked === 'string' ? picked : null;
  }
  return typeof window === 'undefined' ? null : window.prompt('SQL file path', 'C:\\queries\\orders.sql');
}

export async function pickDirectory(current?: string): Promise<string | null> {
  if (inTauri()) {
    const { open } = await import('@tauri-apps/plugin-dialog');
    const picked = await open({ directory: true, multiple: false, defaultPath: current || undefined });
    return typeof picked === 'string' ? picked : null;
  }
  return typeof window === 'undefined' ? null : window.prompt('Folder path', current ?? '');
}

export async function pickAnyFile(filters?: { name: string; extensions: string[] }[]): Promise<string | null> {
  if (inTauri()) {
    const { open } = await import('@tauri-apps/plugin-dialog');
    const picked = await open({ multiple: false, directory: false, filters });
    return typeof picked === 'string' ? picked : null;
  }
  return typeof window === 'undefined' ? null : window.prompt('File path', '');
}

/**
 * Reveal an output file in the OS file manager through the shell's `open_path` command (the
 * mock backend accepts and logs it in the browser).
 */
export async function revealPath(path: string): Promise<void> {
  await openPath(path);
}

/**
 * Open a URL in the default browser (Tauri: the opener plugin, whose capability allows https
 * URLs only) or in a new tab. Never rejects: when the URL cannot be opened (outside the allowed
 * scope, no default browser, a blocked pop-up) the reason is written to the log panel and
 * returned, so the caller can show it next to the link. Resolves to `null` once the URL was
 * handed off.
 */
export async function openUrl(url: string): Promise<string | null> {
  try {
    if (inTauri()) {
      const { openUrl: tauriOpen } = await import('@tauri-apps/plugin-opener');
      await tauriOpen(url);
      return null;
    }
    if (typeof window === 'undefined') throw new Error('no browser window to open it in');
    window.open(url, '_blank', 'noopener');
    return null;
  } catch (e) {
    const message = `Could not open ${url}: ${failureReason(e)}`;
    logLocal('error', message);
    return message;
  }
}

/** Plugin errors arrive as plain strings; everything else goes through `errorText`. */
function failureReason(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  return errorText(e);
}
