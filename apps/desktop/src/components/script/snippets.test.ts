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

  it('python reads from the env or the connections JSON', () => {
    expect(connectionSnippet('python', 'kpia', true)).toContain("os.environ['BULKDB_HOST']");
    expect(connectionSnippet('python', 'kpia2', false)).toContain("c['name'] == 'kpia2'");
  });

  it('bash leaves a comment pointing at the env', () => {
    expect(connectionSnippet('bash', 'kpia', true)).toContain('이 탭의 연결');
    expect(connectionSnippet('bash', 'kpia2', false)).toContain('BULKDB_CONNECTIONS');
  });
});

describe('snippetGroups', () => {
  it('has node query + export + util groups, python and bash their own', () => {
    const node = snippetGroups('node');
    expect(node.flatMap((g) => g.items.map((i) => i.id))).toEqual(expect.arrayContaining(['select', 'raw', 'count', 'xlsx', 'csv', 'xlsx-multi', 'two-db']));
    expect(snippetGroups('python').flatMap((g) => g.items.map((i) => i.id))).toContain('pandas');
    expect(snippetGroups('bash').flatMap((g) => g.items.map((i) => i.id))).toContain('conns');
  });
});

describe('exportCode', () => {
  const sheet = (name: string, columns: string[]): ExportSheet => ({ name, columns });

  it('generates a single-sheet xlsx with explicit columns', () => {
    expect(exportCode([sheet('Sheet1', ['id', 'name'])], 'out.xlsx', 'xlsx')).toBe(
      "const rows = []; // 여기에 행을 채우세요\nawait bulkdb.toXlsx(rows, 'out.xlsx', { columns: ['id', 'name'] });",
    );
  });

  it('omits the columns option when none are given', () => {
    expect(exportCode([sheet('Sheet1', [])], 'out.xlsx', 'xlsx')).toBe('const rows = []; // 여기에 행을 채우세요\nawait bulkdb.toXlsx(rows, \'out.xlsx\');');
  });

  it('generates CSV code', () => {
    expect(exportCode([sheet('Sheet1', ['a'])], 'x.csv', 'csv')).toContain("await bulkdb.toCsv(rows, 'x.csv', { columns: ['a'] });");
  });

  it('generates multi-sheet xlsx with one rows variable per sheet', () => {
    const code = exportCode([sheet('사용자', ['id']), sheet('합계', ['metric', 'value'])], 'book.xlsx', 'xlsx');
    expect(code).toContain('const rows1 = []; // 사용자');
    expect(code).toContain('const rows2 = []; // 합계');
    expect(code).toContain("'사용자': rows1,");
    expect(code).toContain("'합계': rows2,");
    expect(code).toContain("}, 'book.xlsx');");
  });
});
