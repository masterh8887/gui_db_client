import type { HistoryEntry, TableInfo } from 'ipc-types';
import { describe, expect, it } from 'vitest';
import type { AppAction } from '@/lib/actions';
import { mockConnections } from '@/lib/mock-data';
import { buildGroups, connectionSubtitle, historyTitle, parseQuery, type PaletteInput } from './sources';
import { ps } from './strings';

const tables: TableInfo[] = [
  { schema: 'public', name: 'orders', kind: 'table', estimatedRows: BigInt(48_200_000) },
  { schema: 'public', name: 'order_items', kind: 'table', estimatedRows: BigInt(120_000_000) },
  { schema: 'public', name: 'customers', kind: 'table', estimatedRows: BigInt(1_200_000) },
  { schema: 'public', name: 'v_daily', kind: 'view' },
];

const action = (id: string, title: string, extra: Partial<AppAction> = {}): AppAction => ({ id, group: 'app', title: () => title, run: () => undefined, ...extra });
const actions = [action('app.settings', 'Settings', { keywords: ['preferences'] }), action('query.run', 'Run query', { shortcut: 'mod+enter' }), action('theme.dark', 'Dark theme')];

const entry = (id: number, connId: string, label: string, sql = 'SELECT 1'): HistoryEntry => ({
  id: BigInt(id),
  jobId: `j${id}`,
  connId,
  startedAt: '2026-10-26T10:00:00.000Z',
  state: 'done',
  label,
  sql,
  session: {},
  output: { dir: 'C:/x', namePattern: '{name}', format: 'csv', overwrite: false },
  files: [],
});

function input(p: Partial<PaletteInput> = {}): PaletteInput {
  return {
    mode: 'workspace',
    connId: 'c-prod',
    scope: 'all',
    query: '',
    tables: { database: 'orders', list: tables },
    exactCounts: {},
    recentTables: [],
    connections: mockConnections(),
    opened: ['c-prod'],
    history: [entry(1, 'c-prod', 'orders export'), entry(2, 'c-local', 'local dump', 'SELECT * FROM events')],
    actions,
    recentActionIds: [],
    ...p,
  };
}

const ids = (groups: ReturnType<typeof buildGroups>) => groups.map((g) => g.id);

describe('parseQuery', () => {
  it('turns > and @ into the actions and connections scopes', () => {
    expect(parseQuery('all', '> run')).toEqual({ scope: 'actions', text: 'run', prefix: '> ' });
    expect(parseQuery('all', '@prod')).toEqual({ scope: 'connections', text: 'prod', prefix: '@' });
    expect(parseQuery('all', 'orders')).toEqual({ scope: 'all', text: 'orders', prefix: '' });
    // A chip scope keeps the text as typed.
    expect(parseQuery('tables', '>x')).toEqual({ scope: 'tables', text: '>x', prefix: '' });
  });
});

describe('buildGroups', () => {
  it('searches tables, connections, history and actions with the best group first', () => {
    const groups = buildGroups(input({ query: 'order' }));
    expect(groups[0].id).toBe('tables');
    expect(groups[0].items.map((i) => i.title)).toEqual(['orders', 'order_items']);
    expect(groups[0].items[0].indices).toEqual([0, 1, 2, 3, 4]);
    expect(ids(groups)).toContain('history');
  });

  it('matches outside the title (host, SQL, keywords) without highlighting', () => {
    const conns = buildGroups(input({ query: '@db-staging' }));
    expect(conns.map((g) => g.id)).toEqual(['connections']);
    expect(conns[0].items[0].title).toBe('staging (PostgreSQL)');
    expect(conns[0].items[0].indices).toEqual([]);
    const hist = buildGroups(input({ scope: 'history', query: 'events' }));
    expect(hist[0].items.map((i) => i.title)).toEqual(['local dump']);
    const acts = buildGroups(input({ query: '>preferences' }));
    expect(acts[0].items.map((i) => i.title)).toEqual(['Settings']);
  });

  it('offers only connections and actions on Welcome', () => {
    const groups = buildGroups(input({ mode: 'welcome', connId: null, query: 'e' }));
    expect(ids(groups).sort()).toEqual(['actions', 'connections']);
    expect(buildGroups(input({ mode: 'welcome', connId: null, scope: 'tables', query: 'e' }))).toEqual([]);
  });

  it('with an empty query lists recent tables, the other open connections, then recent actions', () => {
    const groups = buildGroups(
      input({
        recentTables: [{ database: 'orders', schema: 'public', name: 'customers' }],
        findTable: () => tables[2],
        opened: ['c-staging', 'c-prod'],
        recentActionIds: ['theme.dark', 'gone.action'],
      }),
    );
    expect(groups.map((g) => g.title)).toEqual([ps('groupRecentTables'), ps('groupOpenConnections'), ps('groupRecentActions')]);
    expect(groups[0].items[0]).toMatchObject({ kind: 'table', title: 'customers', info: tables[2] });
    expect(groups[1].items.map((i) => i.title)).toEqual(['staging (PostgreSQL)']);
    expect(groups[2].items.map((i) => i.title)).toEqual(['Dark theme']);
  });

  it('falls back to every action when none was run yet, and lists every connection on Welcome', () => {
    const groups = buildGroups(input({ mode: 'welcome', connId: null, opened: ['c-local'] }));
    expect(groups.map((g) => g.title)).toEqual([ps('groupConnections'), ps('groupActions')]);
    expect(groups[0].items[0].title).toBe('local (SQLite)');
    expect(groups[0].items).toHaveLength(3);
    expect(groups[1].items).toHaveLength(actions.length);
  });

  it('lists the whole scope for a chip with an empty query', () => {
    expect(buildGroups(input({ scope: 'tables' }))[0].items).toHaveLength(tables.length);
    expect(buildGroups(input({ scope: 'history' }))[0].items).toHaveLength(2);
    expect(ids(buildGroups(input({ scope: 'connections' })))).toEqual(['connections']);
  });

  it('pick mode lists the other connections only', () => {
    const groups = buildGroups(input({ pick: { fromConnId: 'c-prod' } }));
    expect(groups).toHaveLength(1);
    expect(groups[0].title).toBe(ps('groupPick'));
    expect(groups[0].items.map((i) => i.title).sort()).toEqual(['local (SQLite)', 'staging (PostgreSQL)']);
  });

  it('never carries anything but names and estimates for tables', () => {
    const [g] = buildGroups(input({ scope: 'tables', query: 'orders' }));
    const item = g.items[0];
    expect(item).toMatchObject({ kind: 'table', title: 'orders', secondary: 'public', ref: { database: 'orders', schema: 'public', name: 'orders' } });
  });
});

describe('helpers', () => {
  it('describes connections and history entries', () => {
    const [prod, local] = mockConnections();
    expect(connectionSubtitle(prod)).toMatch(/:5432 · orders$/);
    expect(connectionSubtitle(local)).toBe('C:\\data\\local.db');
    expect(historyTitle({ ...entry(3, 'c', ''), sql: '\n  SELECT a\nFROM b' })).toBe('SELECT a');
  });
});
