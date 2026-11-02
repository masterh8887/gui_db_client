import { describe, expect, it } from 'vitest';
import { encodingNotes, parseEncodingNote } from './probeNotes';

describe('probe encoding notes', () => {
  // The exact texts of core `mysql_dispatch::plan_text`.
  it('recognizes the latin1 default and the unsupported pre-4.1 charset', () => {
    expect(parseEncodingNote('default character set is latin1: if Korean text looks broken, set the connection encoding to euc-kr')).toEqual({
      kind: 'latin1Default',
    });
    expect(
      parseEncodingNote("server character set 'sjis' is not supported; text is read as latin1. Choose encoding euc-kr, utf8 or latin1 explicitly"),
    ).toEqual({ kind: 'unsupportedCharset', charset: 'sjis' });
    expect(parseEncodingNote("server character set 'unknown' is not supported; text is read as latin1. Choose ...")).toEqual({
      kind: 'unsupportedCharset',
      charset: 'unknown',
    });
  });

  it('ignores the other probe notes', () => {
    const notes = [
      'MySQL 8.0.36',
      'pre-4.1 server: connected with the BulkDB legacy MySQL client (no TLS, no prepared statements)',
      'default character set is latin1: if Korean text looks broken, set the connection encoding to euc-kr',
      'SHOW GRANTS failed: denied',
    ];
    expect(encodingNotes(notes)).toEqual([{ kind: 'latin1Default' }]);
    expect(encodingNotes([])).toEqual([]);
  });
});
