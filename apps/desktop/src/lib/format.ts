// Formatting helpers. Every numeric input accepts `bigint | number` because ts-rs types Rust
// `u64` as `bigint` while the JSON that actually arrives carries plain numbers.

export type Num = bigint | number | null | undefined;

/** Normalise a `bigint | number | null | undefined` to a `number` (`undefined` when absent). */
export function num(v: Num): number | undefined {
  if (v === null || v === undefined) return undefined;
  return typeof v === 'bigint' ? Number(v) : v;
}


const intFmt = new Intl.NumberFormat('en-US', { maximumFractionDigits: 0 });

/** `1234567` -> `1,234,567`. `undefined` -> `—`. */
export function formatInt(v: Num): string {
  const n = num(v);
  if (n === undefined || !Number.isFinite(n)) return '—';
  return intFmt.format(Math.round(n));
}

/** Row-count style compact number: `1.2K`, `3.4M`, `1.0B`. */
export function formatCompact(v: Num): string {
  const n = num(v);
  if (n === undefined || !Number.isFinite(n)) return '—';
  const abs = Math.abs(n);
  if (abs < 1000) return intFmt.format(Math.round(n));
  const units: [number, string][] = [
    [1e12, 'T'],
    [1e9, 'B'],
    [1e6, 'M'],
    [1e3, 'K'],
  ];
  for (const [div, suffix] of units) {
    if (abs >= div) {
      const val = n / div;
      return `${val >= 100 ? val.toFixed(0) : val.toFixed(1)}${suffix}`;
    }
  }
  return intFmt.format(n);
}

const BYTE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];

/** Binary units: `0 B`, `1.5 KB`, `12.3 MB`, `1.0 GB`. */
export function formatBytes(v: Num, digits = 1): string {
  const n = num(v);
  if (n === undefined || !Number.isFinite(n)) return '—';
  if (Math.abs(n) < 1024) return `${Math.round(n)} B`;
  let val = Math.abs(n);
  let i = 0;
  while (val >= 1024 && i < BYTE_UNITS.length - 1) {
    val /= 1024;
    i += 1;
  }
  return `${n < 0 ? '-' : ''}${val.toFixed(digits)} ${BYTE_UNITS[i]}`;
}

/** Parse `64MB`, `1 GB`, `512kB`, `8192` (kB default, like PostgreSQL) into bytes. */
export function parseMemSize(s: string | undefined | null): number | undefined {
  if (!s) return undefined;
  const m = /^\s*(\d+(?:\.\d+)?)\s*([kmgt]?b?)\s*$/i.exec(s);
  if (!m) return undefined;
  const n = parseFloat(m[1]);
  const unit = m[2].toLowerCase();
  const mult: Record<string, number> = {
    '': 1024,
    b: 1,
    k: 1024,
    kb: 1024,
    m: 1024 ** 2,
    mb: 1024 ** 2,
    g: 1024 ** 3,
    gb: 1024 ** 3,
    t: 1024 ** 4,
    tb: 1024 ** 4,
  };
  const f = mult[unit];
  return f === undefined ? undefined : Math.round(n * f);
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/**
 * Elapsed time: `0.8s`, `12s`, `1m 05s`, `2h 03m`.
 */
export function formatDuration(ms: Num): string {
  const n = num(ms);
  if (n === undefined || !Number.isFinite(n) || n < 0) return '—';
  if (n < 1000) return `${(n / 1000).toFixed(1)}s`;
  const totalSec = Math.floor(n / 1000);
  if (totalSec < 60) return `${totalSec}s`;
  const min = Math.floor(totalSec / 60);
  const sec = totalSec % 60;
  if (min < 60) return `${min}m ${pad2(sec)}s`;
  const h = Math.floor(min / 60);
  return `${h}h ${pad2(min % 60)}m`;
}

/** ETA in seconds: `~1m 05s`; unknown -> `—`. */
export function formatEta(sec: Num): string {
  const n = num(sec);
  if (n === undefined || !Number.isFinite(n) || n < 0) return '—';
  return `~${formatDuration(n * 1000)}`;
}

/** `12.3 MB/s`. */
export function formatByteRate(bytesPerSec: Num): string {
  const n = num(bytesPerSec);
  if (n === undefined || !Number.isFinite(n)) return '—';
  return `${formatBytes(n)}/s`;
}

/** `45.2K rows/s`. */
export function formatRowRate(rowsPerSec: Num): string {
  const n = num(rowsPerSec);
  if (n === undefined || !Number.isFinite(n)) return '—';
  return `${formatCompact(n)} rows/s`;
}

/** `0..100` clamped, or `undefined` when total is unknown. */
export function percent(done: Num, total: Num): number | undefined {
  const d = num(done);
  const t = num(total);
  if (d === undefined || t === undefined || t <= 0) return undefined;
  return Math.max(0, Math.min(100, (d / t) * 100));
}

/** RFC 3339 -> `2026-10-26 14:03` in local time; invalid input is returned as-is. */
export function formatDateTime(iso: string | undefined | null, withSeconds = false): string {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const date = `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  const time = `${pad2(d.getHours())}:${pad2(d.getMinutes())}${withSeconds ? `:${pad2(d.getSeconds())}` : ''}`;
  return `${date} ${time}`;
}

/** `14:03:07` for log lines. */
export function formatTime(iso: string | undefined | null): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/** Last path segment (both separators). */
export function baseName(path: string): string {
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
  return i >= 0 ? path.slice(i + 1) : path;
}

/** Join a directory and a file name with the separator the directory already uses. */
export function joinPath(dir: string, file: string): string {
  if (!dir) return file;
  const sep = dir.includes('\\') && !dir.includes('/') ? '\\' : '/';
  return dir.endsWith(sep) ? `${dir}${file}` : `${dir}${sep}${file}`;
}
