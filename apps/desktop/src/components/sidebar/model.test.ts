import type { ExactCount, TableInfo } from 'ipc-types';
import { describe, expect, it } from 'vitest';
import { buildItemModel, collapseKey, refOf, rowAriaLabel, rowTooltip, rowTrailing, sameRef, sectionOf } from './model';
import { ts } from './strings';

const tb = (name: string, kind: TableInfo['kind'] = 'table', rows?: number, extra: Partial<TableInfo> = {}): TableInfo => ({
  schema: 'public',
  name,
  kind,
  estimatedRows: rows === undefined ? undefined : BigInt(rows),
  ...extra,
});

const exact: ExactCount = {
  table: { database: 'orders', schema: 'public', name: 'orders' },
  rows: BigInt(48_213_004),
  elapsedMs: BigInt(12_300),
  at: '2026-10-27T13:02:11',
};

const TABLES = [
  tb('orders', 'table', 48_200_000),
  tb('v_daily', 'view'),
  tb('events', 'partitioned', 1_250_000_000),
  tb('monthly', 'materialized_view', 120),
  tb('remote', 'foreign'),
  tb('odd', 'other'),
  tb('order_items', 'table', 215_000_000),
];

describe('sidebar model', () => {
  it('groups by kind in section order, partitioned tables under 테이블, catalog order inside', () => {
    const m = buildItemModel(TABLES, '', {}, () => false);
    expect(m.sections.map((s) => s.key)).toEqual(['table', 'view', 'materialized_view', 'foreign', 'other']);
    expect(m.sections[0].rows.map((r) => r.table.name)).toEqual(['orders', 'events', 'order_items']);
    expect(m.visible.map((t) => t.name)).toEqual(['orders', 'events', 'order_items', 'v_daily', 'monthly', 'remote', 'odd']);
    expect(sectionOf('partitioned')).toBe('table');
    expect(m.matchCount).toBe(7);
  });

  it('hides empty sections and marks only sections with an estimated row', () => {
    const m = buildItemModel([tb('a', 'table', 5), tb('v', 'view')], '', {}, () => false);
    expect(m.sections.map((s) => [s.key, s.hasEstimate])).toEqual([
      ['table', true],
      ['view', false],
    ]);
    // A section whose only estimate is replaced by an exact count has nothing estimated left.
    const allExact = buildItemModel([tb('a', 'table', 5)], '', {}, () => true);
    expect(allExact.sections[0].hasEstimate).toBe(false);
  });

  it('skips collapsed sections in the keyboard order, except while searching', () => {
    const collapsed = { [collapseKey('table')]: true };
    const m = buildItemModel(TABLES, '', collapsed, () => false);
    expect(m.sections[0].collapsed).toBe(true);
    expect(m.visible.map((t) => t.name)).toEqual(['v_daily', 'monthly', 'remote', 'odd']);

    const searching = buildItemModel(TABLES, 'ord', collapsed, () => false);
    expect(searching.sections[0].collapsed).toBe(false);
    expect(searching.visible.map((t) => t.name)).toEqual(['orders', 'order_items']);
    expect(searching.sections[0].rows[0].indices).toEqual([0, 1, 2]);
    expect(buildItemModel(TABLES, 'zzz', {}, () => false).matchCount).toBe(0);
  });

  it('builds refs in the browsed database and compares them', () => {
    const ref = refOf('orders', tb('orders'));
    expect(ref).toEqual({ database: 'orders', schema: 'public', name: 'orders' });
    expect(refOf(null, tb('x'))).toEqual({ schema: 'public', name: 'x' });
    expect(sameRef(ref, { database: 'orders', schema: 'public', name: 'orders' })).toBe(true);
    expect(sameRef(ref, { database: 'analytics_dw', schema: 'public', name: 'orders' })).toBe(false);
    expect(sameRef(null, ref)).toBe(false);
  });

  it('labels every count: ≈ estimate, exact count with its date, size as the last resort', () => {
    const orders = tb('orders', 'table', 48_213_000, { comment: 'One row per order', totalBytes: BigInt(6_100_000_000) });
    expect(rowTrailing(orders, undefined)).toBe('≈48.2M');
    expect(rowTrailing(orders, exact)).toBe('48,213,004 · 10-27');
    expect(rowTrailing(tb('v', 'view', undefined, { totalBytes: BigInt(2048) }), undefined)).toMatch(/KB|kB/);
    expect(rowTrailing(tb('v', 'view'), undefined)).toBeUndefined();

    const tip = rowTooltip(orders, undefined).split('\n');
    expect(tip[0]).toBe('public.orders');
    expect(tip[1]).toBe('One row per order');
    expect(tip[2]).toBe(ts('rowEstimatedTip', { rows: '48,213,000' }));
    expect(tip[3]).toMatch(/GB/);
    expect(rowTooltip(orders, exact)).toContain(ts('rowExactTip', { rows: '48,213,004', at: '2026-10-27 13:02' }));
    expect(rowTooltip(tb('v', 'view'), undefined)).toContain(ts('rowNoEstimate'));

    expect(rowAriaLabel(orders, undefined)).toBe(ts('ariaEstimated', { name: 'orders', rows: '48,213,000' }));
    expect(rowAriaLabel(orders, exact)).toBe(ts('ariaExact', { name: 'orders', rows: '48,213,004', at: '2026-10-27 13:02' }));
    expect(rowAriaLabel(tb('v', 'view'), undefined)).toBe('v');
  });
});
