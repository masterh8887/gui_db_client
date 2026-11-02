// Every issue code the copy can show has a Korean title with an English fallback
// (docs/db-copy.md 2 "Issue conventions", 7 "Strings"): the codes core emits (read from the Rust
// sources of `crates/core/src/copy`, outside their tests) and the warning / blocker codes of
// sections 3 and 5 of the spec. A new code in core without a title fails here.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { getLang, setLang } from '@/lib/i18n';
import { issueTitle } from './model';
import { cs } from './strings';

const ROOT = resolve(process.cwd(), '../..');
/** `CoreError::code()` values: error payloads, never plan issues. */
const ERROR_CODES = new Set(['CONNECT', 'AUTH', 'QUERY', 'IO', 'CANCELLED', 'TIMEOUT', 'DRIVER_MISSING', 'OOM_RISK', 'INVALID', 'TUNNEL', 'NOT_FOUND', 'INTERNAL']);
/** `check_work_mem` and `concurrent_session_issue` of the export preflight, added to copy plans. */
const EXPORT_PREFLIGHT_CODES = ['WORK_MEM_HIGH', 'WORK_MEM_INVALID', 'CONCURRENT_SESSION', 'SESSIONS_UNKNOWN'];

function rustFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    if (statSync(p).isDirectory()) return rustFiles(p);
    return p.endsWith('.rs') ? [p] : [];
  });
}

/** Issue codes in core's copy sources, test modules left out. */
function coreCodes(): Set<string> {
  const out = new Set<string>();
  const patterns = [
    // issue("CODE", …), info / warning / danger("CODE", …), also across lines
    /\b(?:issue|info|warning|danger)\(\s*"([A-Z][A-Z0-9_]+)"/g,
    // pub const CODE_X: &str = "CODE" (runtime codes, value fixes)
    /pub(?:\(crate\))? const [A-Z0-9_]+: &str = "([A-Z][A-Z0-9_]+)"/g,
    // ("CODE", "message") pairs picked by a match, e.g. the zero-date policy
    /\(\s*"([A-Z][A-Z0-9]*_[A-Z0-9_]+)",\s*(?:format!\()?"/g,
    // AckNeed { code: "CODE".into(), … }
    /\bcode: "([A-Z][A-Z0-9_]+)"\.into\(\)/g,
  ];
  for (const file of rustFiles(join(ROOT, 'crates/core/src/copy'))) {
    if (/(?:^|[\\/])(?:\w+_)?tests?\.rs$|testkit\.rs$/.test(file)) continue;
    const src = readFileSync(file, 'utf8');
    const cut = src.indexOf('#[cfg(test)]');
    const body = cut >= 0 ? src.slice(0, cut) : src;
    for (const re of patterns) for (const m of body.matchAll(re)) out.add(m[1]);
    // for code in ["A", "B"] (codes read back from an error message) and the named code lists
    const lists = [...body.matchAll(/for code in \[([^\]]+)\]/g), ...body.matchAll(/(?:ENV_CODES|DDL_ONLY): &\[&str\] = &\[([^\]]+)\]/g)];
    for (const m of lists) for (const c of m[1].matchAll(/"([A-Z][A-Z0-9_]+)"/g)) out.add(c[1]);
  }
  // Export preflight checks the copy reuses (crates/core/src/preflight.rs, through copy/preflight.rs).
  for (const c of EXPORT_PREFLIGHT_CODES) out.add(c);
  for (const c of ERROR_CODES) out.delete(c);
  return out;
}

/** Warning / blocker codes of spec sections 3 (translation) and 5 (execution). */
function specCodes(): Set<string> {
  const doc = readFileSync(join(ROOT, 'docs/db-copy.md'), 'utf8');
  const part = (from: string, to: string) => doc.slice(doc.indexOf(from), doc.indexOf(to));
  const text = part('## 3. Translation rules', '## 4. Data paths') + part('## 5. Execution', '## 6. Facade');
  const out = new Set<string>();
  // `CODE` (warning …) / (item blocker …) / (info …)
  for (const m of text.matchAll(/`([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*)`[^`\n]{0,3}\((?:item )?(?:blocker|warning|info|danger)/g)) out.add(m[1]);
  // The 5.3 table: | `CODE`, `CODE` | severity | …
  for (const line of text.split('\n')) {
    if (!/^\| `[A-Z]/.test(line) || !/\| *(?:item )?(?:blocker|warning|info|danger)/.test(line)) continue;
    for (const m of line.split('|')[1].matchAll(/`([A-Z][A-Z0-9_]+)`/g)) out.add(m[1]);
  }
  return out;
}

const titled = new Set((cs.keys as readonly string[]).filter((k) => k.startsWith('issue.')).map((k) => k.slice('issue.'.length)));

describe('copy issue titles', () => {
  it('reads a plausible set of codes from core and the spec', () => {
    const core = coreCodes();
    const spec = specCodes();
    // Guards against a pattern that silently stops matching.
    expect(core.size).toBeGreaterThan(80);
    expect(spec.size).toBeGreaterThan(60);
    for (const c of ['SRC_LATIN1_KOREAN', 'TABLES_SKIPPED_EXISTING', 'PLAN_CHANGED', 'ZERO_DATE_KEEP', 'RUN_IN_USE', 'ROW_TOO_LARGE', 'DATABASE_CREATED']) expect(core, c).toContain(c);
    for (const c of ['FK_OTHER_DATABASE', 'PG_EXTENSION_VERSION', 'TRIGGER_UNREADABLE', 'UTF8MB3_ONLY', 'BLOB_PACKET_LIMIT', 'TZ_DST']) expect([...core, ...spec], c).toContain(c);
    expect([...core].filter((c) => ERROR_CODES.has(c))).toEqual([]);
  });

  it('every code core can emit has a title', () => {
    expect([...coreCodes()].filter((c) => !titled.has(c)).sort()).toEqual([]);
  });

  it('every warning and blocker code of spec sections 3 and 5 has a title', () => {
    expect([...specCodes()].filter((c) => !titled.has(c)).sort()).toEqual([]);
  });

  it('titles are Korean with English fallbacks; unknown codes show the message', () => {
    const prev = getLang();
    try {
      setLang('ko');
      expect(issueTitle({ code: 'DATABASE_CREATED', message: 'x' })).toBe('대상 데이터베이스를 새로 만듭니다');
      setLang('en');
      expect(issueTitle({ code: 'DATABASE_CREATED', message: 'x' })).toBe('The target database is created');
      expect(issueTitle({ code: 'NO_SUCH_CODE', message: 'the English message' })).toBe('the English message');
    } finally {
      setLang(prev);
    }
  });
});
