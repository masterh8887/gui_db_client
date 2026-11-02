// Which characters EUC-KR (CP949, WHATWG "euc-kr") can represent, for the mock backend's
// SQL_ENCODING / CSV_ENCODING checks (core: `encoding::find_unrepresentable`). The table is built
// once from the platform's own `TextDecoder('euc-kr')` by decoding every two-byte sequence, so it
// matches encoding_rs; without that decoder (a build without ICU) a conservative fallback
// accepts ASCII, Hangul syllables and compatibility jamo only. All 11,172 Hangul syllables are
// always accepted: some ICU builds (Node's) decode only KS X 1001, not the UHC extension.

let table: Set<number> | null | undefined;

function buildTable(): Set<number> | null {
  let decoder: TextDecoder;
  try {
    decoder = new TextDecoder('euc-kr', { fatal: false });
  } catch {
    return null;
  }
  // Every lead byte 0x81..0xFE with every possible trail byte, each pair followed by '\n'.
  const bytes: number[] = [];
  for (let lead = 0x81; lead <= 0xfe; lead++) {
    for (let trail = 0x41; trail <= 0xfe; trail++) bytes.push(lead, trail, 0x0a);
  }
  const set = new Set<number>();
  for (const piece of decoder.decode(new Uint8Array(bytes)).split('\n')) {
    // A valid pair decodes to exactly one character; an invalid one gives U+FFFD (+ the trail).
    const cp = piece.codePointAt(0);
    if (cp !== undefined && piece.length === 1 && cp !== 0xfffd) set.add(cp);
  }
  return set.size > 2000 ? set : null;
}

/** Whether CP949 can represent `ch` (one code point). */
export function cp949Has(ch: string): boolean {
  const cp = ch.codePointAt(0);
  if (cp === undefined) return true;
  if (cp < 0x80) return true;
  if (table === undefined) table = buildTable();
  if (cp >= 0xac00 && cp <= 0xd7a3) return true;
  if (table) return table.has(cp);
  return cp >= 0x3131 && cp <= 0x318e;
}

/** The first character of `text` CP949 cannot represent, with its 1-based line / column (in characters). */
export function firstNonCp949(text: string): { ch: string; line: number; column: number } | null {
  let line = 1;
  let column = 0;
  for (const ch of text) {
    if (ch === '\n') {
      line += 1;
      column = 0;
      continue;
    }
    column += 1;
    if (!cp949Has(ch)) return { ch, line, column };
  }
  return null;
}

/** `'😀' (U+1F600)`, the way the core's `Unrepresentable::describe` names a character. */
export function describeChar(ch: string): string {
  const cp = ch.codePointAt(0) ?? 0;
  return `'${ch}' (U+${cp.toString(16).toUpperCase().padStart(4, '0')})`;
}
