import { describe, expect, it } from 'vitest';
import { fuzzyFilter, fuzzyMatch } from './fuzzy';

describe('fuzzy', () => {
  it('matches in order, case-insensitively, and returns the matched positions', () => {
    expect(fuzzyMatch('ord', 'Orders')?.indices).toEqual([0, 1, 2]);
    expect(fuzzyMatch('oi', 'order_items')?.indices).toEqual([0, 6]);
    expect(fuzzyMatch('xyz', 'orders')).toBeNull();
    expect(fuzzyMatch('', 'orders')).toEqual({ score: 0, indices: [] });
  });

  it('prefers substrings, word starts and short texts', () => {
    const items = ['customer_orders', 'orders', 'order_items', 'o_r_d'];
    // Substrings first (shorter text wins), then the scattered match; `o_r_d` has no `ers`.
    expect(fuzzyFilter(items, 'orders', (s) => s).map((r) => r.item)).toEqual(['orders', 'customer_orders', 'order_items']);
    expect(fuzzyFilter(items, 'oi', (s) => s)[0].item).toBe('order_items');
    expect(fuzzyFilter(items, '', (s) => s).map((r) => r.item)).toEqual(items);
    expect(fuzzyFilter(items, 'o', (s) => s, 2)).toHaveLength(2);
  });

  it('supports ^ and $ anchors and a contains-only mode', () => {
    expect(fuzzyMatch('^ord', 'customer_orders')).toBeNull();
    expect(fuzzyMatch('^ord', 'orders')).not.toBeNull();
    expect(fuzzyMatch('ers$', 'orders')?.indices).toEqual([3, 4, 5]);
    expect(fuzzyMatch('ers$', 'orders_x')).toBeNull();
    expect(fuzzyMatch('odr', 'orders', 'contains')).toBeNull();
    expect(fuzzyMatch('der', 'orders', 'contains')?.indices).toEqual([2, 3, 4]);
  });
});
