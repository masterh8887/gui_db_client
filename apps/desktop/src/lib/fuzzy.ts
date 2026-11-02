// Fuzzy matching for the palette, the Welcome list and the sidebar search (DESIGN.md §7.5).
//
// `fuzzyMatch` finds the query's characters in order (case-insensitive) and scores the match:
// consecutive runs, word starts (after `_ . - / space`, a digit boundary or a camelCase hump)
// and an early first hit score higher; long gaps and long texts score lower. A plain substring
// always beats a scattered match. `^` at the start of the query anchors it to the start of the
// text, `$` at the end to the end. Mode `contains` accepts substrings only.

export interface FuzzyMatch {
  score: number;
  /** Positions in `text` of the matched characters (for `Highlight`). */
  indices: number[];
}

const SEPARATORS = new Set(['_', '.', '-', '/', '\\', ' ', ':', '@', '(', ')', ',']);

function isBoundary(text: string, i: number): boolean {
  if (i === 0) return true;
  const prev = text[i - 1];
  const cur = text[i];
  if (SEPARATORS.has(prev)) return true;
  if (prev >= 'a' && prev <= 'z' && cur >= 'A' && cur <= 'Z') return true;
  const prevDigit = prev >= '0' && prev <= '9';
  const curDigit = cur >= '0' && cur <= '9';
  return prevDigit !== curDigit;
}

function scoreIndices(text: string, indices: number[], substring: boolean): number {
  let score = substring ? 100 : 0;
  for (let k = 0; k < indices.length; k += 1) {
    const i = indices[k];
    score += 10;
    if (isBoundary(text, i)) score += 8;
    if (k > 0) {
      const gap = i - indices[k - 1] - 1;
      if (gap === 0) score += 12;
      else score -= Math.min(gap, 20) * 0.75;
    }
  }
  if (indices.length > 0) score -= Math.min(indices[0], 30) * 0.5;
  score -= text.length * 0.05;
  return score;
}

/** Earliest in-order positions of `lq` in `lt` (always finds one when it exists). */
function greedy(lt: string, lq: string): number[] | null {
  const out: number[] = [];
  let pos = 0;
  for (const ch of lq) {
    const i = lt.indexOf(ch, pos);
    if (i < 0) return null;
    out.push(i);
    pos = i + 1;
  }
  return out;
}

/** In-order positions that jump to word starts when the next character is not adjacent. */
function boundaryFirst(text: string, lt: string, lq: string): number[] | null {
  const out: number[] = [];
  let pos = 0;
  for (let k = 0; k < lq.length; k += 1) {
    const ch = lq[k];
    if (k > 0 && lt[pos] === ch) {
      out.push(pos);
      pos += 1;
      continue;
    }
    let pick = -1;
    for (let i = pos; i < lt.length; i += 1) {
      if (lt[i] === ch && isBoundary(text, i)) {
        pick = i;
        break;
      }
    }
    if (pick < 0) pick = lt.indexOf(ch, pos);
    if (pick < 0) return null;
    out.push(pick);
    pos = pick + 1;
  }
  return out;
}

export function fuzzyMatch(query: string, text: string, mode: 'fuzzy' | 'contains' = 'fuzzy'): FuzzyMatch | null {
  let q = query.trim();
  const anchorStart = q.startsWith('^');
  if (anchorStart) q = q.slice(1);
  const anchorEnd = q.length > 0 && q.endsWith('$');
  if (anchorEnd) q = q.slice(0, -1);
  if (!q) return { score: 0, indices: [] };
  const lt = text.toLowerCase();
  const lq = q.toLowerCase();

  // Substring first: the best kind of match, and the only one in `contains` mode.
  let at = -1;
  if (anchorStart && anchorEnd) at = lt === lq ? 0 : -1;
  else if (anchorStart) at = lt.startsWith(lq) ? 0 : -1;
  else if (anchorEnd) at = lt.endsWith(lq) ? lt.length - lq.length : -1;
  else {
    // Prefer an occurrence at a word start.
    let from = 0;
    while (from <= lt.length - lq.length) {
      const i = lt.indexOf(lq, from);
      if (i < 0) break;
      if (at < 0) at = i;
      if (isBoundary(text, i)) {
        at = i;
        break;
      }
      from = i + 1;
    }
  }
  if (at >= 0) {
    const indices = Array.from({ length: lq.length }, (_, k) => at + k);
    return { score: scoreIndices(text, indices, true), indices };
  }
  if (mode === 'contains') return null;

  const candidates = [greedy(lt, lq), boundaryFirst(text, lt, lq)].filter(
    (c): c is number[] =>
      c !== null && (!anchorStart || c[0] === 0) && (!anchorEnd || c[c.length - 1] === lt.length - 1),
  );
  if (candidates.length === 0) return null;
  let best: FuzzyMatch | null = null;
  for (const indices of candidates) {
    const score = scoreIndices(text, indices, false);
    if (!best || score > best.score) best = { score, indices };
  }
  return best;
}

/**
 * Items whose `key` matches `query`, best first (stable for equal scores). An empty query keeps
 * every item in its order. `limit` caps the result.
 */
export function fuzzyFilter<T>(
  items: readonly T[],
  query: string,
  key: (t: T) => string,
  limit?: number,
): { item: T; match: FuzzyMatch }[] {
  const out: { item: T; match: FuzzyMatch; i: number }[] = [];
  items.forEach((item, i) => {
    const match = fuzzyMatch(query, key(item));
    if (match) out.push({ item, match, i });
  });
  if (query.trim()) out.sort((a, b) => b.match.score - a.match.score || a.i - b.i);
  const sliced = limit !== undefined ? out.slice(0, limit) : out;
  return sliced.map(({ item, match }) => ({ item, match }));
}
