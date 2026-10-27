import { act, fireEvent, render, screen } from '@testing-library/react';
import { EditorView } from '@codemirror/view';
import type { ColumnInfo, TableInfo } from 'ipc-types';
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { setLang } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { resetStores } from '@/test/stores';
import { minimalChange, SqlEditor } from './SqlEditor';
import { completionState, rankCompletions } from './sqlCompletion';
import { buildSqlSchema, dialectNameFor, referencedTables } from './sqlSchema';

// jsdom has no layout: CodeMirror's tooltip positioning asks ranges for their client rects.
// Minimal stand-ins, local to this file (the shared setup stays untouched).
const emptyRects = () => Object.assign([], { item: () => null }) as unknown as DOMRectList;
const zeroRect = () => ({ x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: () => ({}) }) as DOMRect;
const rangeProto = Range.prototype as Partial<Range>;
const hadRects = 'getClientRects' in Range.prototype;

beforeAll(() => {
  if (!hadRects) {
    rangeProto.getClientRects = emptyRects;
    rangeProto.getBoundingClientRect = zeroRect;
  }
  setLang('en');
});

afterAll(() => {
  if (!hadRects) {
    delete rangeProto.getClientRects;
    delete rangeProto.getBoundingClientRect;
  }
  setLang('ko');
});

function col(position: number, name: string, dataType: string): ColumnInfo {
  return { position, name, dataType, nullable: true, primaryKey: position === 1, unique: false };
}

function table(schema: string, name: string): TableInfo {
  return { schema, name, kind: 'table' };
}

function editorView(container: HTMLElement): EditorView {
  const dom = container.querySelector<HTMLElement>('.cm-editor');
  if (!dom) throw new Error('no editor');
  const view = EditorView.findFromDOM(dom);
  if (!view) throw new Error('no view');
  return view;
}

/** Type `text` at the end of the document the way CodeMirror's input handling tags it. */
function typeAtEnd(view: EditorView, text: string) {
  act(() => {
    const end = view.state.doc.length;
    view.dispatch({ changes: { from: end, insert: text }, selection: { anchor: end + text.length }, userEvent: 'input.type' });
  });
}

function optionLabels(): string[] {
  return screen.queryAllByRole('option').map((o) => o.querySelector('.cm-sql-completion-label')?.textContent ?? '');
}

describe('SqlEditor', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('renders the initial value with line numbers and the dialect of the selected connection', () => {
    const { container } = render(<SqlEditor value={'SELECT id\nFROM public.orders'} onChange={vi.fn()} aria-label="SQL" />);
    const textbox = screen.getByRole('textbox', { name: 'SQL' });
    expect(textbox).toHaveTextContent('SELECT id');
    expect(textbox).toHaveTextContent('FROM public.orders');
    const gutter = [...container.querySelectorAll('.cm-lineNumbers .cm-gutterElement')].map((e) => e.textContent);
    expect(gutter).toEqual(expect.arrayContaining(['1', '2']));
    expect(container.querySelector('[data-dialect]')).toHaveAttribute('data-dialect', 'PostgreSQL');
    // Keywords are highlighted with the theme's token classes.
    expect(container.querySelector('.cm-sql-keyword')).toHaveTextContent('SELECT');

    act(() => useConnectionsStore.setState({ selectedId: 'c-local' }));
    expect(container.querySelector('[data-dialect]')).toHaveAttribute('data-dialect', 'SQLite');
  });

  it('reports user edits and applies external values without echoing them or moving the cursor', () => {
    const onChange = vi.fn();
    const { container, rerender } = render(<SqlEditor value="SELECT 1" onChange={onChange} />);
    const view = editorView(container);

    typeAtEnd(view, ' AS one');
    expect(onChange).toHaveBeenLastCalledWith('SELECT 1 AS one');
    expect(onChange).toHaveBeenCalledTimes(1);

    // The parent echoes the edit back: same text, nothing dispatched.
    rerender(<SqlEditor value="SELECT 1 AS one" onChange={onChange} />);
    expect(view.state.doc.toString()).toBe('SELECT 1 AS one');

    // An external change elsewhere keeps the cursor where the user left it.
    act(() => view.dispatch({ selection: { anchor: 3 } }));
    rerender(<SqlEditor value="SELECT 1 AS one -- from history" onChange={onChange} />);
    expect(view.state.doc.toString()).toBe('SELECT 1 AS one -- from history');
    expect(view.state.selection.main.head).toBe(3);
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('fires onRun on Mod-Enter and onExplain on Mod-E, preventing the browser default', () => {
    const onRun = vi.fn();
    const onExplain = vi.fn();
    const { container } = render(<SqlEditor value="SELECT 1" onChange={vi.fn()} onRun={onRun} onExplain={onExplain} aria-label="SQL" />);
    const textbox = screen.getByRole('textbox', { name: 'SQL' });

    expect(fireEvent.keyDown(textbox, { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true })).toBe(false);
    expect(onRun).toHaveBeenCalledTimes(1);
    expect(onExplain).not.toHaveBeenCalled();

    expect(fireEvent.keyDown(textbox, { key: 'e', code: 'KeyE', keyCode: 69, ctrlKey: true })).toBe(false);
    expect(onExplain).toHaveBeenCalledTimes(1);
    expect(onRun).toHaveBeenCalledTimes(1);

    // Mod-Enter never inserts a line.
    expect(editorView(container).state.doc.toString()).toBe('SELECT 1');
  });

  it('leaves Mod-Enter / Mod-E to the app-wide shortcuts when no callback is given', () => {
    const { container } = render(<SqlEditor value="SELECT 1" onChange={vi.fn()} aria-label="SQL" />);
    const textbox = screen.getByRole('textbox', { name: 'SQL' });
    expect(fireEvent.keyDown(textbox, { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true })).toBe(true);
    expect(fireEvent.keyDown(textbox, { key: 'e', code: 'KeyE', keyCode: 69, ctrlKey: true })).toBe(true);
    expect(editorView(container).state.doc.toString()).toBe('SELECT 1');
  });

  it('is read-only when asked, and the run key still works there', () => {
    const onRun = vi.fn();
    const { container, rerender } = render(<SqlEditor value="SELECT 1" onChange={vi.fn()} onRun={onRun} readOnly aria-label="SQL" />);
    const textbox = screen.getByRole('textbox', { name: 'SQL' });
    expect(editorView(container).state.readOnly).toBe(true);
    expect(textbox).toHaveAttribute('aria-readonly', 'true');
    fireEvent.keyDown(textbox, { key: 'Enter', code: 'Enter', keyCode: 13, ctrlKey: true });
    expect(onRun).toHaveBeenCalledTimes(1);

    rerender(<SqlEditor value="SELECT 1" onChange={vi.fn()} onRun={onRun} aria-label="SQL" />);
    expect(editorView(container).state.readOnly).toBe(false);
    expect(textbox).not.toHaveAttribute('aria-readonly');
  });

  it('completes schema, table and column names from the catalog cache without loading anything', () => {
    useCatalogStore.setState({
      databases: { 'c-prod': [{ name: 'orders', accessible: true, isCurrent: true }] },
      schemas: { 'c-prod/orders': [{ name: 'public' }, { name: 'analytics' }] },
      tables: {
        'c-prod/orders/public': [table('public', 'orders'), table('public', 'customers')],
        'c-prod/orders/analytics': [table('analytics', 'daily_sales')],
      },
      details: {
        'c-prod/orders/public/orders': { columns: [col(1, 'id', 'bigint'), col(2, 'customer_id', 'bigint'), col(3, 'placed_at', 'timestamptz')] },
      },
    });
    const onChange = vi.fn();
    const { container } = render(<SqlEditor value="" onChange={onChange} aria-label="SQL" />);
    const view = editorView(container);
    const textbox = screen.getByRole('textbox', { name: 'SQL' });
    mockBackend.calls.length = 0;

    // Table names of the default schema complete unqualified.
    typeAtEnd(view, 'SELECT * FROM ord');
    expect(screen.getByRole('listbox')).toBeInTheDocument();
    expect(optionLabels()[0]).toBe('orders');
    expect(textbox).toHaveAttribute('aria-activedescendant');
    fireEvent.keyDown(textbox, { key: 'Enter', code: 'Enter', keyCode: 13 });
    expect(view.state.doc.toString()).toBe('SELECT * FROM orders');
    expect(onChange).toHaveBeenLastCalledWith('SELECT * FROM orders');
    expect(screen.queryByRole('listbox')).toBeNull();

    // Columns of the FROM table complete unqualified.
    typeAtEnd(view, ' WHERE cust');
    expect(optionLabels()).toEqual(expect.arrayContaining(['customer_id']));
    fireEvent.keyDown(textbox, { key: 'Escape', code: 'Escape', keyCode: 27 });
    expect(view.state.field(completionState)).toBeNull();

    // Through an alias, and without keywords after the qualifier.
    act(() => view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: 'SELECT * FROM public.orders o WHERE ' } }));
    typeAtEnd(view, 'o.pl');
    expect(optionLabels()).toEqual(['placed_at']);
    fireEvent.keyDown(textbox, { key: 'Escape', code: 'Escape', keyCode: 27 });

    // `schema.` lists that schema's tables; arrows move the selection.
    act(() => view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: '' } }));
    typeAtEnd(view, 'SELECT * FROM analytics.');
    expect(optionLabels()).toEqual(['daily_sales']);
    fireEvent.keyDown(textbox, { key: 'Tab', code: 'Tab', keyCode: 9 });
    expect(view.state.doc.toString()).toBe('SELECT * FROM analytics.daily_sales');

    // Ctrl-Space opens the list explicitly.
    typeAtEnd(view, ' ');
    fireEvent.keyDown(textbox, { key: ' ', code: 'Space', keyCode: 32, ctrlKey: true });
    expect(view.state.field(completionState)?.explicit).toBe(true);
    fireEvent.keyDown(textbox, { key: 'ArrowDown', code: 'ArrowDown', keyCode: 40 });
    expect(view.state.field(completionState)?.selected).toBe(1);
    // Typing something that ends the word closes it again.
    typeAtEnd(view, ',');
    expect(view.state.field(completionState)).toBeNull();

    expect(mockBackend.calls.filter((c) => c.startsWith('catalog_'))).toEqual([]);
  });
});

describe('SQL completion helpers', () => {
  it('minimalChange keeps the common prefix and suffix', () => {
    expect(minimalChange('SELECT a FROM t', 'SELECT a, b FROM t')).toEqual({ from: 8, to: 8, insert: ', b' });
    expect(minimalChange('abc', 'abc')).toEqual({ from: 3, to: 3, insert: '' });
    expect(minimalChange('abc', '')).toEqual({ from: 0, to: 3, insert: '' });
  });

  it('ranks exact, prefix, word-boundary and substring matches in that order', () => {
    const ranked = rankCompletions(
      [
        { label: 'customer_orders', type: 'type' },
        { label: 'ORDER', type: 'keyword', boost: -1 },
        { label: 'orders', type: 'type' },
        { label: 'ord', type: 'type' },
        { label: 'coordinates', type: 'type' },
        { label: 'orders', type: 'type' },
        { label: 'items', type: 'type' },
      ],
      'ord',
    ).map((c) => c.label);
    expect(ranked).toEqual(['ord', 'orders', 'ORDER', 'customer_orders', 'coordinates']);
  });

  it('maps drivers to lang-sql dialects', () => {
    expect(['postgres', 'mysql', 'mssql', 'sqlite', 'oracle', 'odbc', undefined].map(dialectNameFor)).toEqual([
      'PostgreSQL',
      'MySQL',
      'MSSQL',
      'SQLite',
      'PLSQL',
      'StandardSQL',
      'StandardSQL',
    ]);
  });

  it('builds the completion schema per driver from the cached catalog', () => {
    const pg = buildSqlSchema(
      {
        databases: { c1: [{ name: 'app', accessible: true, isCurrent: true }, { name: 'other', accessible: true, isCurrent: false }] },
        schemas: { 'c1/app': [{ name: 'public' }], 'c1/other': [{ name: 'hidden' }] },
        tables: { 'c1/app/public': [table('public', 'Order Items')], 'c1/other/hidden': [table('hidden', 'x')] },
        details: {},
      },
      { connId: 'c1', driver: 'postgres' },
    );
    expect(pg.defaultSchema).toBe('public');
    expect(Object.keys(pg.namespace)).toEqual(['public']);
    // Names that need quoting complete quoted.
    const items = (pg.namespace as Record<string, { children: Record<string, { self: { apply?: string } }> }>).public.children['Order Items'];
    expect(items.self.apply).toBe('"Order Items"');

    const mysql = buildSqlSchema(
      {
        databases: { c2: [{ name: 'shop', accessible: true, isCurrent: true }, { name: 'crm', accessible: true, isCurrent: false }] },
        schemas: { 'c2/shop': [{ name: 'shop' }], 'c2/crm': [{ name: 'crm' }] },
        tables: { 'c2/shop/shop': [table('shop', 'orders')], 'c2/crm/crm': [table('crm', 'leads')] },
        details: {},
      },
      { connId: 'c2', driver: 'mysql', database: 'shop' },
    );
    expect(Object.keys(mysql.namespace).sort()).toEqual(['crm', 'shop']);
    expect(mysql.defaultSchema).toBe('shop');

    const mssql = buildSqlSchema(
      {
        databases: {},
        schemas: { 'c3/Sales': [{ name: 'dbo' }], 'c3/Archive': [{ name: 'dbo' }] },
        tables: { 'c3/Sales/dbo': [table('dbo', 'Orders')], 'c3/Archive/dbo': [table('dbo', 'Orders2019')] },
        details: { 'c3/Sales/dbo/Orders': { columns: [col(1, 'OrderID', 'int')] } },
      },
      { connId: 'c3', driver: 'mssql', database: 'Sales' },
    );
    expect(Object.keys(mssql.namespace).sort()).toEqual(['Archive', 'dbo']);
    expect(mssql.defaultSchema).toBe('dbo');
    expect(mssql.tables.find((t) => t.name === 'Orders')?.columns?.map((c) => c.name)).toEqual(['OrderID']);

    expect(buildSqlSchema({ databases: {}, schemas: {}, tables: {}, details: {} }, { connId: null }).tables).toEqual([]);
  });

  it('finds the tables a query reads', () => {
    expect(referencedTables('SELECT * FROM public.orders o JOIN "Sales"."Order Items" i ON i.id = o.id LEFT JOIN customers c ON true')).toEqual([
      { schema: 'public', name: 'orders' },
      { schema: 'Sales', name: 'Order Items' },
      { schema: undefined, name: 'customers' },
    ]);
  });
});
