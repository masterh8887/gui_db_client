import { describe, expect, it } from 'vitest';
import { defaultOutputOpts } from '@/stores/extract';
import { middleTruncate } from './parts';
import { isStoppable, outputSummary } from './QueryBar';
import { supportedSessionKeys } from './SessionOptionsForm';

describe('query helpers', () => {
  it('middle-truncates long paths and keeps short ones', () => {
    expect(middleTruncate('C:\\exports\\a.csv', 64)).toBe('C:\\exports\\a.csv');
    const long = `D:\\exports\\${'x'.repeat(80)}_20261027.csv`;
    const cut = middleTruncate(long, 24);
    expect(cut).toHaveLength(24);
    expect(cut.startsWith('D:\\exports')).toBe(true);
    expect(cut.endsWith('.csv')).toBe(true);
    expect(cut).toContain('…');
  });

  it('summarises the output options and the full path', () => {
    const out = { ...defaultOutputOpts(), dir: 'D:\\exports', namePattern: '{conn}_{name}', compress: 'gzip' as const };
    expect(outputSummary(out)).toEqual({ text: expect.stringMatching(/^CSV/), path: 'D:\\exports\\{conn}_{name}.csv.gz' });
    expect(outputSummary(out).text).toContain('gzip');
    const pq = outputSummary({ ...out, format: 'parquet', compress: undefined });
    expect(pq.path.endsWith('.parquet')).toBe(true);
    expect(pq.text.startsWith('Parquet')).toBe(true);
  });

  it('names EUC-KR and shows BOM only when one is written (UTF-8)', () => {
    const csv = { header: true, delimiter: ',', nullAs: '', bom: true, crlf: true };
    const utf8 = outputSummary({ ...defaultOutputOpts(), csv });
    expect(utf8.text).toBe('CSV · BOM');
    // `--excel --encoding euc-kr` history: bom:true + euc-kr writes no BOM.
    const euc = outputSummary({ ...defaultOutputOpts(), csv: { ...csv, encoding: 'euc-kr' } });
    expect(euc.text).toBe('CSV · EUC-KR');
    expect(euc.path.endsWith('.csv')).toBe(true);
  });

  it('treats only the busy job states as stoppable', () => {
    expect(['pending', 'preflight', 'running', 'finishing'].every((s) => isStoppable(s as never))).toBe(true);
    expect(['warned', 'done', 'failed', 'cancelled', 'rejected'].some((s) => isStoppable(s as never))).toBe(false);
    expect(isStoppable(undefined)).toBe(false);
  });

  it('maps registry session options to form keys', () => {
    expect(supportedSessionKeys(undefined).size).toBe(5);
    const keys = supportedSessionKeys({ sessionOpts: ['max_execution_time', 'fetch_batch', 'net_read_timeout'] } as never);
    expect([...keys].sort()).toEqual(['fetchBatchRows', 'statementTimeoutMs']);
  });
});
