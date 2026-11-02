// Recognize the encoding notes of a MySQL probe (core `mysql_dispatch::plan_text`, put into
// `ServerInfo.notes` by `mysql_common::probe`) so the connection sheet can localize them and
// point to 호환성 > 서버 인코딩 (docs/mysql-compat.md §5, §9). The core writes them only with
// `encoding = auto`.

export type EncodingNote =
  /** 4.1+: `default character set is latin1: if Korean text looks broken, set the connection encoding to euc-kr`. */
  | { kind: 'latin1Default' }
  /** pre-4.1: `server character set 'x' is not supported; text is read as latin1. Choose encoding ...`. */
  | { kind: 'unsupportedCharset'; charset: string };

export function parseEncodingNote(note: string): EncodingNote | null {
  const n = note.trim();
  if (n.startsWith('default character set is latin1:')) return { kind: 'latin1Default' };
  const m = /^server character set '(.*?)' is not supported; text is read as latin1\b/.exec(n);
  return m ? { kind: 'unsupportedCharset', charset: m[1] } : null;
}

/** The encoding notes among a probe's notes, in order. */
export function encodingNotes(notes: readonly string[]): EncodingNote[] {
  return notes.map(parseEncodingNote).filter((n): n is EncodingNote => n !== null);
}
