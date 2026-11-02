import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { PlanNode, PlanSummary, PlanWarning } from 'ipc-types';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { setLang } from '@/lib/i18n';
import { ancestorIds, costShare, matchWarnings, planTotalCost } from './planModel';
import { PlanSummaryView } from './PlanSummaryView';
import { PlanTree } from './PlanTree';

beforeAll(() => setLang('en'));
afterAll(() => setLang('ko'));

function node(nodeType: string, extra: Partial<PlanNode> = {}, children: PlanNode[] = []): PlanNode {
  return { nodeType, children, extra: {}, ...extra };
}

const ordersScan = node('Seq Scan', {
  relation: { schema: 'public', name: 'orders' },
  alias: 'o',
  planRows: 12_000_000,
  planWidth: 48,
  totalCost: 400,
  actualRows: 11_800_000,
  actualLoops: 1,
  actualTimeMs: 1234.5,
  extra: { Filter: "(status = 'paid')" },
});
const customersScan = node('Seq Scan', { relation: { schema: 'public', name: 'customers' }, alias: 'c', planRows: 50_000, planWidth: 16, totalCost: 40 });
const hash = node('Hash', { planRows: 50_000, planWidth: 16, totalCost: 50, parentRelationship: 'Inner' }, [customersScan]);
const join = node('Hash Join', { planRows: 2_000_000, planWidth: 64, totalCost: 600, extra: { 'Hash Cond': '(o.customer_id = c.id)' } }, [ordersScan, hash]);
const root = node('Sort', { planRows: 2_000_000, planWidth: 64, totalCost: 1000, startupCost: 980, extra: { 'Sort Key': 'o.placed_at' } }, [join]);

const warnings: PlanWarning[] = [
  {
    code: 'PLAN_SORT_SPILL',
    severity: 'warning',
    message: 'Sort over about 2,000,000 rows x 64 B (~122.1 MB) exceeds work_mem 4 MB; it will spill to temp files on the server.',
    detail: 'o.placed_at',
  },
  { code: 'PLAN_SEQ_SCAN_LARGE', severity: 'info', message: 'Sequential scan of public.orders: about 12,000,000 rows.' },
  { code: 'WORK_MEM_UNKNOWN', severity: 'info', message: 'work_mem could not be read from the server.' },
];

function plan(overrides: Partial<PlanSummary> = {}): PlanSummary {
  return {
    estimatedRows: BigInt(2_000_000),
    estimatedWidth: 64,
    totalCost: 1000,
    estimatedBytes: BigInt(128_000_000),
    analyzed: false,
    elapsedMs: BigInt(42),
    warnings,
    tables: [
      { schema: 'public', name: 'orders' },
      { schema: 'public', name: 'customers' },
    ],
    root,
    raw: '[{"Plan": {"Node Type": "Sort"}}]',
    rawFormat: 'json',
    ...overrides,
  };
}

const item = (name: string) => screen.getByRole('treeitem', { name });
const queryItem = (name: string) => screen.queryByRole('treeitem', { name });

describe('PlanTree', () => {
  it('renders the nested plan with relations, estimates, costs and extra chips', () => {
    render(<PlanTree plan={plan()} />);
    expect(screen.getByRole('tree', { name: 'Plan tree' })).toBeInTheDocument();
    expect(screen.getAllByRole('treeitem')).toHaveLength(5);
    expect(screen.getByText('5 nodes')).toBeInTheDocument();

    const sort = item('Sort');
    expect(sort).toHaveAttribute('aria-level', '1');
    expect(item('Hash Join')).toHaveAttribute('aria-level', '2');
    expect(item('Seq Scan public.customers')).toHaveAttribute('aria-level', '4');

    const orders = item('Seq Scan public.orders');
    const ordersRow = orders.firstElementChild as HTMLElement;
    expect(within(ordersRow).getByText('public.orders')).toBeInTheDocument();
    expect(within(ordersRow).getByText('o')).toBeInTheDocument(); // alias
    expect(ordersRow).toHaveTextContent('12,000,000 rows');
    expect(ordersRow).toHaveTextContent('× 48 B');
    expect(ordersRow).toHaveTextContent('400 (40%)');
    expect(within(ordersRow).getByRole('img', { name: '40% of the total cost' })).toBeInTheDocument();
    const filter = ordersRow.querySelector('[data-extra="Filter"]');
    expect(filter).toHaveTextContent("Filter(status = 'paid')");

    expect(within(sort.firstElementChild as HTMLElement).getByRole('img', { name: '100% of the total cost' })).toBeInTheDocument();
    expect(item('Hash')).toHaveTextContent('Inner');
  });

  it('collapses and expands nodes, one at a time or all at once', async () => {
    const user = userEvent.setup();
    render(<PlanTree plan={plan()} />);

    await user.click(screen.getByRole('button', { name: 'Collapse – Hash Join' }));
    expect(item('Hash Join')).toHaveAttribute('aria-expanded', 'false');
    expect(queryItem('Seq Scan public.orders')).toBeNull();
    expect(queryItem('Hash')).toBeNull();

    await user.click(screen.getByRole('button', { name: 'Expand – Hash Join' }));
    expect(item('Hash Join')).toHaveAttribute('aria-expanded', 'true');
    expect(item('Seq Scan public.orders')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Collapse all' }));
    expect(screen.getAllByRole('treeitem')).toHaveLength(1);
    expect(item('Sort')).toHaveAttribute('aria-expanded', 'false');

    await user.click(screen.getByRole('button', { name: 'Expand all' }));
    expect(screen.getAllByRole('treeitem')).toHaveLength(5);
  });

  it('shows actual rows / loops / time only for EXPLAIN ANALYZE', () => {
    const { rerender, container } = render(<PlanTree plan={plan()} />);
    expect(container.querySelector('[data-actual]')).toBeNull();
    expect(screen.queryByText('actual')).toBeNull();

    rerender(<PlanTree plan={plan({ analyzed: true })} />);
    const row = item('Seq Scan public.orders').firstElementChild as HTMLElement;
    expect(row.querySelector('[data-actual="rows"]')).toHaveTextContent('11,800,000 rows');
    expect(row).toHaveTextContent('× 1 loops');
    expect(row).toHaveTextContent('1s');
    // Nodes without actuals show none even when analyzed.
    expect((item('Sort').firstElementChild as HTMLElement).querySelector('[data-actual]')).toBeNull();
  });

  it('lists warnings on top and highlights the nodes they name', async () => {
    const user = userEvent.setup();
    render(<PlanTree plan={plan()} />);

    const sort = item('Sort');
    expect(sort).toHaveAttribute('data-warned', 'warning');
    expect(within(sort.firstElementChild as HTMLElement).getByText('PLAN_SORT_SPILL')).toBeInTheDocument();
    expect(item('Seq Scan public.orders')).toHaveAttribute('data-warned', 'info');
    expect(item('Seq Scan public.customers')).not.toHaveAttribute('data-warned');
    expect(item('Hash Join')).not.toHaveAttribute('data-warned');

    // All three on top; the one naming no node has no "show node" button.
    for (const w of warnings) expect(screen.getAllByText(w.code).length).toBeGreaterThan(0);
    expect(screen.getAllByRole('button', { name: 'Show node' })).toHaveLength(2);
    expect(screen.getAllByText('WORK_MEM_UNKNOWN')).toHaveLength(1);

    // "Show node" reveals a collapsed node.
    await user.click(screen.getByRole('button', { name: 'Collapse all' }));
    expect(queryItem('Seq Scan public.orders')).toBeNull();
    await user.click(screen.getAllByRole('button', { name: 'Show node' })[1]);
    expect(item('Seq Scan public.orders')).toBeInTheDocument();
    expect(queryItem('Seq Scan public.customers')).toBeNull();
  });

  it('toggles the raw plan text', async () => {
    const user = userEvent.setup();
    render(<PlanTree plan={plan()} />);
    expect(screen.queryByText('[{"Plan": {"Node Type": "Sort"}}]')).toBeNull();
    const toggle = screen.getByRole('button', { name: 'Raw plan' });
    await user.click(toggle);
    expect(toggle).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByText('[{"Plan": {"Node Type": "Sort"}}]').tagName).toBe('PRE');
    await user.click(toggle);
    expect(screen.queryByText('[{"Plan": {"Node Type": "Sort"}}]')).toBeNull();
  });

  it('starts expanded again for a new plan', async () => {
    const user = userEvent.setup();
    const { rerender } = render(<PlanTree plan={plan()} />);
    await user.click(screen.getByRole('button', { name: 'Collapse all' }));
    expect(screen.getAllByRole('treeitem')).toHaveLength(1);
    rerender(<PlanTree plan={plan({ elapsedMs: BigInt(50) })} />);
    expect(screen.getAllByRole('treeitem')).toHaveLength(5);
  });
});

describe('PlanSummaryView', () => {
  it('shows the summary and the tree when the plan has a root', () => {
    render(<PlanSummaryView plan={plan()} />);
    expect(screen.getByText('2,000,000')).toBeInTheDocument();
    expect(screen.getAllByText('public.orders').length).toBeGreaterThan(0);
    expect(screen.getByRole('tree')).toBeInTheDocument();
  });

  it('renders a SQLite-style plan that has no rows, widths or costs at all', () => {
    // Shape of a real `EXPLAIN QUERY PLAN` result from the SQLite driver: optional numbers are
    // absent (the core omits `None` fields on the wire), the root is a synthetic "QUERY PLAN".
    const sqlite: PlanSummary = {
      analyzed: false,
      elapsedMs: BigInt(1),
      warnings: [],
      tables: [{ name: 't' }],
      root: node('QUERY PLAN', {}, [
        node('SCAN', { relation: { name: 't' }, extra: { detail: 'SCAN t' } }),
        node('USE TEMP B-TREE FOR ORDER BY', { extra: { detail: 'USE TEMP B-TREE FOR ORDER BY' } }),
      ]),
      raw: 'SCAN t\nUSE TEMP B-TREE FOR ORDER BY',
      rawFormat: 'text',
    };
    render(<PlanSummaryView plan={sqlite} />);
    expect(screen.getByText('SCAN')).toBeInTheDocument();
    // Node label plus its `detail` chip.
    expect(screen.getAllByText('USE TEMP B-TREE FOR ORDER BY').length).toBeGreaterThanOrEqual(1);
  });

  it('shows warnings and the raw text when the plan has no root', async () => {
    const user = userEvent.setup();
    render(<PlanSummaryView plan={plan({ root: undefined, raw: 'SCAN orders', rawFormat: 'text' })} />);
    expect(screen.queryByRole('tree')).toBeNull();
    expect(screen.getByText('PLAN_SORT_SPILL')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Show node' })).toBeNull();
    await user.click(screen.getByRole('button', { name: 'Raw plan' }));
    expect(screen.getByText('SCAN orders')).toBeInTheDocument();
  });
});

describe('planModel', () => {
  it('matches warnings by relation, then by node type (longest name, row count)', () => {
    const sortA = node('Sort', { planRows: 10 });
    const sortB = node('Sort', { planRows: 5_000_000 });
    const tree = node('Hash Join', {}, [node('Hash', {}, [sortA]), sortB]);
    const m = matchWarnings(tree, [
      { code: 'A', severity: 'warning', message: 'Hash Join over a large input' },
      { code: 'B', severity: 'warning', message: 'Sort over about 5,000,000 rows spills' },
      { code: 'C', severity: 'danger', message: 'the result may be far larger than expected' },
    ]);
    expect(m.nodesOf[0]).toEqual(['0']); // not the plain "Hash" node
    expect(m.nodesOf[1]).toEqual(['0.1']); // the Sort with that row count
    expect(m.nodesOf[2]).toEqual([]);
    expect(m.byNode.get('0')?.map((w) => w.code)).toEqual(['A']);
  });

  it('keeps sort/hash spill warnings on the Sort/Hash node when their keys are table-qualified', () => {
    // Sort(5,000,000) -> Hash Join -> [Seq Scan public.orders, Hash -> Seq Scan public.customers]
    const tree = node('Sort', { planRows: 5_000_000, planWidth: 64, extra: { 'Sort Key': 'orders.created_at' } }, [
      node('Hash Join', { planRows: 5_000_000, extra: { 'Hash Cond': '(orders.customer_id = customers.id)' } }, [
        node('Seq Scan', { planRows: 12_000_000, relation: { schema: 'public', name: 'orders' }, alias: 'orders' }),
        node('Hash', { planRows: 50_000 }, [node('Seq Scan', { planRows: 50_000, relation: { schema: 'public', name: 'customers' }, alias: 'customers' })]),
      ]),
    ]);
    const m = matchWarnings(tree, [
      {
        code: 'PLAN_SORT_SPILL',
        severity: 'warning',
        message: 'Sort over about 5,000,000 rows x 64 B (~305.2 MB) exceeds work_mem 4 MB; it will spill to temp files on the server.',
        detail: 'orders.created_at',
      },
      {
        code: 'PLAN_SORT_SPILL',
        severity: 'warning',
        message: 'Hash over about 50,000 rows x 16 B (~781.3 KB) exceeds work_mem 64 KB; it will spill to temp files on the server.',
        detail: '(orders.customer_id = customers.id)',
      },
      { code: 'PLAN_SEQ_SCAN_LARGE', severity: 'info', message: 'Sequential scan of public.orders: about 12,000,000 rows.' },
      { code: 'X', severity: 'info', message: 'Seq Scan on public.customers reads every row' },
    ]);
    expect(m.nodesOf).toEqual([['0'], ['0.0.1'], ['0.0.0'], ['0.0.1.0']]);
    expect(m.byNode.get('0')?.map((w) => w.detail)).toEqual(['orders.created_at']);
    expect(m.byNode.get('0.0.0')?.map((w) => w.code)).toEqual(['PLAN_SEQ_SCAN_LARGE']);
  });

  it('uses a named relation only to narrow same-type nodes to the one above its scan', () => {
    const tree = node('Append', {}, [
      node('Sort', { planRows: 1_000 }, [node('Seq Scan', { planRows: 1_000, relation: { schema: 'public', name: 'orders' } })]),
      node('Sort', { planRows: 1_000 }, [node('Seq Scan', { planRows: 1_000, relation: { schema: 'public', name: 'customers' } })]),
    ]);
    const m = matchWarnings(tree, [
      { code: 'PLAN_SORT_SPILL', severity: 'warning', message: 'Sort over about 1,000 rows exceeds work_mem', detail: 'customers.name' },
      { code: 'PLAN_SORT_SPILL', severity: 'warning', message: 'Sort over about 1,000 rows exceeds work_mem', detail: 'name' },
    ]);
    expect(m.nodesOf).toEqual([['0.1'], ['0.0', '0.1']]);
  });

  it('falls back to the node kind a warning code implies when it points at one node', () => {
    const tree = node('Sort', { planRows: 1_250_000_000 }, [
      node('Nested Loop', { planRows: 1_250_000_000 }, [
        node('Seq Scan', { planRows: 1_250_000_000, relation: { schema: 'public', name: 'events' } }),
        node('Limit', { planRows: 1 }, [node('Index Scan', { planRows: 4, relation: { schema: 'public', name: 'order_items' } })]),
      ]),
    ]);
    const m = matchWarnings(tree, [
      { code: 'LATERAL_PATTERN', severity: 'danger', message: 'LATERAL subquery runs once per outer row (1,250,000,000 rows)', detail: 'add a LIMIT' },
      { code: 'SEQ_SCAN_LARGE', severity: 'warning', message: 'Sequential scan of 1,250,000,000 rows (no WHERE clause)' },
      { code: 'SORT_SPILL', severity: 'warning', message: 'Sorting 1,250,000,000 rows will spill to disk' },
      { code: 'HASH_LARGE', severity: 'warning', message: 'Hash join / aggregate over ~1.2G rows' },
    ]);
    expect(m.nodesOf).toEqual([['0.0'], ['0.0.0'], ['0'], []]);
  });

  it('computes ancestors, totals and shares', () => {
    expect(ancestorIds('0')).toEqual([]);
    expect(ancestorIds('0.1.2')).toEqual(['0', '0.1']);
    expect(planTotalCost(root, undefined)).toBe(1000);
    expect(planTotalCost(undefined, undefined)).toBeUndefined();
    expect(costShare(ordersScan, 1000)).toBeCloseTo(0.4);
    expect(costShare(node('Result'), 1000)).toBeUndefined();
  });
});
