import { describe, expect, it } from 'vitest';
import { connIdent, connectionSnippet, exportCode, snippetGroups, type ExportSheet } from './snippets';

describe('connIdent', () => {
  it('turns a connection name into a valid JS identifier', () => {
    expect(connIdent('kpia')).toBe('kpia');
    expect(connIdent('kpia (prod)')).toBe('kpia_prod');
    expect(connIdent('2024-report')).toBe('_2024_report');
    expect(connIdent('한글 연결')).toBe('db'); // non-ASCII collapses away -> fallback
    expect(connIdent('')).toBe('db');
  });
});

describe('connectionSnippet', () => {
  it('node opens the connection by name with a safe identifier', () => {
    expect(connectionSnippet('node', 'kpia', false)).toBe("const kpia = bulkdb.db('kpia');");
    expect(connectionSnippet('node', "o'brien", false)).toBe("const o_brien = bulkdb.db('o\\'brien');");
  });

  it('python opens the connection by name (sync bulkdb)', () => {
    expect(connectionSnippet('python', 'kpia', true)).toBe("kpia = bulkdb.db('kpia')");
    expect(connectionSnippet('python', 'kpia (prod)', false)).toBe("kpia_prod = bulkdb.db('kpia (prod)')");
  });

  it('bash leaves a comment pointing at the env', () => {
    expect(connectionSnippet('bash', 'kpia', true)).toContain('이 탭의 연결');
    expect(connectionSnippet('bash', 'kpia2', false)).toContain('BULKDB_CONNECTIONS');
  });
});

describe('snippetGroups', () => {
  it('node and python share the bulkdb catalog ids; bash has its own', () => {
    const ids = (r: 'node' | 'python') => snippetGroups(r).flatMap((g) => g.items.map((i) => i.id));
    expect(ids('node')).toEqual(expect.arrayContaining(['select', 'raw', 'count', 'xlsx', 'csv', 'xlsx-multi', 'two-db']));
    expect(ids('python')).toEqual(expect.arrayContaining(['select', 'raw', 'count', 'xlsx', 'csv', 'xlsx-multi', 'two-db']));
    // Python uses the sync bulkdb API (from_, no await).
    const select = snippetGroups('python').flatMap((g) => g.items).find((i) => i.id === 'select')!;
    expect(select.code).toContain("bulkdb.from_('테이블')");
    expect(select.code).not.toContain('await');
    expect(snippetGroups('bash').flatMap((g) => g.items.map((i) => i.id))).toContain('conns');
  });
});

describe('exportCode (node)', () => {
  const sheet = (name: string, columns: string[]): ExportSheet => ({ name, columns });

  it('generates a single-sheet xlsx with explicit columns', () => {
    expect(exportCode([sheet('Sheet1', ['id', 'name'])], 'out.xlsx', 'xlsx', 'node')).toBe(
      "const rows = []; // 여기에 행을 채우세요\nawait bulkdb.toXlsx(rows, 'out.xlsx', { columns: ['id', 'name'] });",
    );
  });

  it('omits the columns option when none are given', () => {
    expect(exportCode([sheet('Sheet1', [])], 'out.xlsx', 'xlsx', 'node')).toBe('const rows = []; // 여기에 행을 채우세요\nawait bulkdb.toXlsx(rows, \'out.xlsx\');');
  });

  it('generates CSV code', () => {
    expect(exportCode([sheet('Sheet1', ['a'])], 'x.csv', 'csv', 'node')).toContain("await bulkdb.toCsv(rows, 'x.csv', { columns: ['a'] });");
  });

  it('generates multi-sheet xlsx with one rows variable per sheet', () => {
    const code = exportCode([sheet('사용자', ['id']), sheet('합계', ['metric', 'value'])], 'book.xlsx', 'xlsx', 'node');
    expect(code).toContain('const rows1 = []; // 사용자');
    expect(code).toContain('const rows2 = []; // 합계');
    expect(code).toContain("'사용자': rows1,");
    expect(code).toContain("'합계': rows2,");
    expect(code).toContain("}, 'book.xlsx');");
  });
});

describe('exportCode (python)', () => {
  const sheet = (name: string, columns: string[]): ExportSheet => ({ name, columns });

  it('generates a bulkdb.to_xlsx single sheet with columns', () => {
    const code = exportCode([sheet('Sheet1', ['id', 'name'])], 'out.xlsx', 'xlsx', 'python');
    expect(code).toContain("bulkdb.to_xlsx(rows, 'out.xlsx', columns=['id', 'name'])");
    expect(code).not.toContain('await');
  });

  it('generates a bulkdb.to_csv', () => {
    const code = exportCode([sheet('Sheet1', ['id'])], 'x.csv', 'csv', 'python');
    expect(code).toContain("bulkdb.to_csv(rows, 'x.csv', columns=['id'])");
  });

  it('generates a bulkdb.to_xlsx dict for multiple sheets', () => {
    const code = exportCode([sheet('사용자', ['id']), sheet('합계', [])], 'book.xlsx', 'xlsx', 'python');
    expect(code).toContain('rows1 = []  # 사용자');
    expect(code).toContain('rows2 = []  # 합계');
    expect(code).toContain("'사용자': rows1,");
    expect(code).toContain("'합계': rows2,");
    expect(code).toContain("}, 'book.xlsx')");
  });
});
