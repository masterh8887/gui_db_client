// Host platform helpers for the WebView. The only place in the UI that looks at the OS.

interface NavigatorUAData {
  platform?: string;
}

function uaPlatform(): string {
  if (typeof navigator === 'undefined') return '';
  const data = (navigator as Navigator & { userAgentData?: NavigatorUAData }).userAgentData;
  if (data?.platform) return data.platform;
  return navigator.platform || navigator.userAgent || '';
}

/** `true` on macOS (and iOS WebViews, which share the Cmd convention). */
export function isMac(): boolean {
  return /mac|iphone|ipad|ipod/i.test(uaPlatform());
}

export function isWindows(): boolean {
  // `Win32` (navigator.platform), `Windows` (userAgentData / userAgent). Not a bare /win/,
  // which would also match `Darwin`.
  return /^win|windows/i.test(uaPlatform());
}

/** Label for the primary modifier: `Cmd` on macOS, `Ctrl` elsewhere. */
export function modKeyLabel(): string {
  return isMac() ? 'Cmd' : 'Ctrl';
}

/** Symbol for the primary modifier: `⌘` on macOS, `Ctrl` elsewhere. */
export function modKeySymbol(): string {
  return isMac() ? '⌘' : 'Ctrl';
}

/** Whether the event carries the platform's primary modifier (Cmd on macOS, Ctrl elsewhere). */
export function hasModKey(e: { metaKey: boolean; ctrlKey: boolean }): boolean {
  return isMac() ? e.metaKey : e.ctrlKey;
}

/** `Ctrl+Enter` / `⌘+Enter` style label. */
export function shortcutLabel(key: string, mod = true): string {
  return mod ? `${modKeySymbol()}+${key}` : key;
}
