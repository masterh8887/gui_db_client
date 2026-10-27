// Browser storage that never throws. `window.localStorage` can be missing (server render during
// `next build`, Node's own experimental `localStorage` global shadowing jsdom's), or its accessor
// can throw (blocked site data, private windows). Persisted UI conveniences such as panel sizes
// must degrade to "not remembered" instead of crashing the app, so every caller goes through
// `safeLocalStorage()`, which falls back to a process-local in-memory `Storage`.

/** A minimal in-memory implementation of the DOM `Storage` interface. */
export class MemoryStorage implements Storage {
  private readonly map = new Map<string, string>();

  get length(): number {
    return this.map.size;
  }

  clear(): void {
    this.map.clear();
  }

  getItem(key: string): string | null {
    return this.map.has(key) ? (this.map.get(key) as string) : null;
  }

  key(index: number): string | null {
    let i = 0;
    for (const k of this.map.keys()) {
      if (i === index) return k;
      i += 1;
    }
    return null;
  }

  removeItem(key: string): void {
    this.map.delete(key);
  }

  setItem(key: string, value: string): void {
    this.map.set(key, String(value));
  }
}

const PROBE_KEY = 'bulkdb-storage-probe';
let fallback: MemoryStorage | null = null;

function probe(s: Storage | undefined | null): s is Storage {
  if (!s || typeof s.setItem !== 'function' || typeof s.getItem !== 'function') return false;
  try {
    s.setItem(PROBE_KEY, '1');
    s.removeItem(PROBE_KEY);
    return true;
  } catch {
    return false;
  }
}

/**
 * The real `window.localStorage` when it exists and accepts writes; otherwise one shared
 * in-memory `Storage` (values then live only until the page reloads).
 */
export function safeLocalStorage(): Storage {
  try {
    const s = typeof window !== 'undefined' ? window.localStorage : undefined;
    if (probe(s)) return s;
  } catch {
    // The accessor itself threw (blocked storage): fall through to memory.
  }
  fallback ??= new MemoryStorage();
  return fallback;
}
