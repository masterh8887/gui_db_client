// Recognize structured `JobResult.notes` written by the core so the UI can localize them.

/**
 * The replaced-characters note of an EUC-KR CSV export (core `encoding::unmappable_note`:
 * `12 character(s) cannot be represented in EUC-KR (CP949) and were written as '?'`).
 */
export function parseUnmappableNote(note: string): { count: bigint; encoding: string } | null {
  const m = /^(\d+) character\(s\) cannot be represented in (.+?) and were written as '\?'$/.exec(note.trim());
  return m ? { count: BigInt(m[1]), encoding: m[2] } : null;
}
