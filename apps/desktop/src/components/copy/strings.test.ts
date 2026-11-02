// The copy string table carries no dead keys (docs/db-copy.md 7 "Strings"): every key in `cs` is
// referenced from the copy components — as a literal `cs('key')` / `cs(k('key'))`, or through a
// dynamic family such as `cs(k(`existing.${p}`))`. A key left behind when its last use went away
// (or a re-added one) fails here, so the table stays in step with the UI.

import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { cs } from './strings';

// vitest runs from apps/desktop; the copy folder holds the only users of `cs`.
const DIR = resolve(process.cwd(), 'src/components/copy');

/** Every non-test TypeScript source of the copy folder (the only place `cs` is used), concatenated. */
function copySource(): string {
  return readdirSync(DIR)
    .filter((f) => (f.endsWith('.ts') || f.endsWith('.tsx')) && f !== 'strings.ts' && !f.includes('.test.'))
    .map((f) => readFileSync(join(DIR, f), 'utf8'))
    .join('\n');
}

describe('copy strings', () => {
  it('has no unused keys', () => {
    const src = copySource();
    // Prefixes of dynamic keys, e.g. `existing.${p}`, `fix.${f.code}`, `issue.${code}`.
    const prefixes = new Set<string>();
    for (const m of src.matchAll(/`([A-Za-z][A-Za-z0-9]*)\.\$\{/g)) prefixes.add(m[1]);
    const used = (key: string): boolean =>
      src.includes(`'${key}'`) ||
      src.includes(`"${key}"`) ||
      src.includes(`\`${key}\``) ||
      (key.includes('.') && prefixes.has(key.slice(0, key.indexOf('.'))));
    const dead = (cs.keys as readonly string[]).filter((k) => !used(k));
    expect(dead).toEqual([]);
  });

  it('keeps the keys the UI stopped using deleted', () => {
    const keys = new Set(cs.keys as readonly string[]);
    for (const dead of ['notWritable', 'planCleared', 'jobState', 'valueFixes', 'otherWindow']) expect(keys.has(dead)).toBe(false);
  });
});
