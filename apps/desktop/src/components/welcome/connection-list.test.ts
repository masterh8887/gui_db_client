import type { Connection } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { bestMatch, connSubtitle, flattenGroups, groupConnections, matchConnections, withOpenGroup } from './connection-list';
import { driverCode } from './DriverBadge';

const drivers = mockDrivers();
let conns: Connection[];

const erp: Connection = {
  id: 'c-erp',
  name: 'erp',
  driver: 'oracle',
  host: 'erp-db.internal',
  port: 1521,
  fields: { service_name: 'ERP' },
  session: {},
  savePassword: true,
  hasPassword: false,
  hasSshSecret: false,
};

describe('Welcome list helpers', () => {
  beforeEach(() => {
    conns = [...mockConnections(), erp];
  });

  it('driver codes: known drivers, then the first two letters of the name', () => {
    expect(['postgres', 'mysql', 'mssql', 'sqlite', 'oracle', 'odbc'].map((d) => driverCode(d))).toEqual(['Pg', 'My', 'Ms', 'Lt', 'Or', 'Od']);
    expect(driverCode('clickhouse', 'ClickHouse')).toBe('Cl');
    expect(driverCode('x-db')).toBe('Xd');
  });

  it('searches name, host, database, driver and tag', () => {
    const names = (q: string) => matchConnections(conns, drivers, q).map((m) => m.conn.id);
    expect(names('')).toHaveLength(4);
    expect(names('db-prod')).toEqual(['c-prod']);
    expect(names('sqlite')).toEqual(['c-local']);
    expect(names('ERP')).toEqual(['c-erp']);
    expect(names('staging')).toEqual(['c-staging']);
    expect(names('Oracle Database')).toEqual(['c-erp']);
    expect(names('nothing-like-this')).toEqual([]);
    // Name hits carry the matched positions for highlighting.
    const prod = matchConnections(conns, drivers, 'prod').find((m) => m.conn.id === 'c-prod');
    expect(prod?.nameIndices).toEqual([0, 1, 2, 3]);
    expect(bestMatch(matchConnections(conns, drivers, 'loc'))?.conn.id).toBe('c-local');
  });

  it('groups by tag in risk order (untagged last) and by driver name', () => {
    const all = matchConnections(conns, drivers, '');
    const byTag = groupConnections(all, 'tag', drivers, false);
    expect(byTag.map((g) => g.key)).toEqual(['tag:production', 'tag:staging', 'tag:local', 'tag:none']);
    expect(byTag[0].label).toBe('production — 운영');
    const byDriver = groupConnections(all, 'driver', drivers, false);
    expect(byDriver.map((g) => g.label)).toEqual(['Oracle Database', 'PostgreSQL', 'SQLite']);
    expect(byDriver[1].items.map((m) => m.conn.id)).toEqual(['c-prod', 'c-staging']);
    const flat = groupConnections(all, 'none', drivers, false);
    expect(flat).toHaveLength(1);
    expect(flat[0].label).toBeNull();
    expect(flattenGroups(flat).map((c) => c.name)).toEqual(['erp', 'local (SQLite)', 'prod (PostgreSQL)', 'staging (PostgreSQL)']);
  });

  it('second line: host:port · database · SSH user@host; SQLite shows the path', () => {
    const [prod, local, staging] = mockConnections();
    expect(connSubtitle(prod)).toEqual({ text: 'db-prod.internal:5432 · orders · SSH deploy@bastion.example.com', path: false });
    expect(connSubtitle(local)).toEqual({ text: 'C:\\data\\local.db', path: true });
    expect(connSubtitle(staging).text).toBe('db-staging.internal:5432 · orders');
    expect(connSubtitle(erp).text).toBe('erp-db.internal:1521 · ERP');
    // An ODBC connection string is never shown (it may carry credentials).
    const odbc = { ...erp, driver: 'odbc', host: undefined, port: undefined, fields: { driver: 'SQL Server', connection_string: 'PWD=secret' } };
    expect(connSubtitle(odbc).text).toBe('SQL Server');
  });

  it('withOpenGroup lists connections open in any window first (name order only)', () => {
    const groups = groupConnections(matchConnections(conns, drivers, ''), 'none', drivers, false);
    const open = withOpenGroup(groups, 'none', false, new Set(['c-staging']));
    expect(open.map((g) => [g.key, g.items.map((m) => m.conn.id)])).toEqual([
      ['open', ['c-staging']],
      ['rest', ['c-erp', 'c-local', 'c-prod']],
    ]);
    expect(flattenGroups(open).map((c) => c.id)).toEqual(['c-staging', 'c-erp', 'c-local', 'c-prod']);
    // All open: no empty second group.
    expect(withOpenGroup(groups, 'none', false, new Set(['c-staging', 'c-erp', 'c-local', 'c-prod'])).map((g) => g.key)).toEqual(['open']);
    // Unchanged: nothing open, searching, other groupings, unknown ids.
    expect(withOpenGroup(groups, 'none', false, new Set())).toEqual(groups);
    expect(withOpenGroup(groups, 'none', true, new Set(['c-staging']))).toEqual(groups);
    expect(withOpenGroup(groups, 'none', false, new Set(['gone']))).toEqual(groups);
    const byTag = groupConnections(matchConnections(conns, drivers, ''), 'tag', drivers, false);
    expect(withOpenGroup(byTag, 'tag', false, new Set(['c-staging']))).toEqual(byTag);
  });
});
