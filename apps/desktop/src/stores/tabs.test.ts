import { beforeEach, describe, expect, it } from 'vitest';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { resetStores } from '@/test/stores';
import { useConnectionsStore } from './connections';
import { isDocDirty, useExtractStore } from './extract';
import { selectActiveQueryTarget, selectActiveTab, selectTabList, tabTitle, useTabsStore, type TableTab } from './tabs';

const orders = { database: 'orders', schema: 'public', name: 'orders' };
const customers = { database: 'orders', schema: 'public', name: 'customers' };

const tabs = () => useTabsStore.getState();
const list = (connId = 'c-prod') => selectTabList(tabs(), connId);
const ids = (connId = 'c-prod') => list(connId).map((t) => t.id);
const active = (connId = 'c-prod') => tabs().sets[connId]?.activeId;

describe('tabs store', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('opens query tabs after the active one, activates them and numbers them by the highest open seq', () => {
    const a = tabs().openQuery('c-prod');
    const b = tabs().openQuery('c-prod');
    expect([a, b]).toEqual(['q1', 'q2']);
    expect(active()).toBe('q2');
    tabs().activate(a);
    const c = tabs().openQuery('c-prod');
    // Inserted right after the active tab.
    expect(ids()).toEqual(['q1', 'q3', 'q2']);
    expect(list().map((t) => (t.kind === 'query' ? t.seq : 0))).toEqual([1, 3, 2]);
    expect(active()).toBe(c);
    // The doc's id is the tab id and it belongs to the tab's connection.
    expect(useExtractStore.getState().docs[c]).toMatchObject({ tabId: c, connId: 'c-prod', panel: 'output', panelCollapsed: true });
    tabs().close('q3');
    tabs().close('q2');
    // Highest open seq + 1 (not a global counter).
    tabs().openQuery('c-prod');
    expect(list().map((t) => (t.kind === 'query' ? t.seq : 0))).toEqual([1, 2]);
    expect(tabTitle(list()[1])).toBe(tabTitle({ id: 'x', kind: 'query', connId: 'c-prod', seq: 2 }));
  });

  it('starts ids again after a store reset (nextId)', () => {
    tabs().openQuery('c-prod');
    tabs().openQuery('c-prod');
    resetStores();
    expect(tabs().openQuery('c-prod')).toBe('q1');
  });

  it('replaces the preview tab with a new tab at the same index, not pushed to closed', () => {
    const q = tabs().openQuery('c-prod');
    const p1 = tabs().openTable('c-prod', orders, { preview: true });
    tabs().activate(q);
    const q2 = tabs().openQuery('c-prod');
    expect(ids()).toEqual([q, q2, p1]);
    const p2 = tabs().openTable('c-prod', customers, { preview: true });
    expect(p2).not.toBe(p1);
    expect(ids()).toEqual([q, q2, p2]);
    expect(tabs().tabs[p1]).toBeUndefined();
    expect(tabs().sets['c-prod'].closed).toHaveLength(0);
    expect(active()).toBe(p2);
  });

  it('activates (and pins) an open tab of the same table; newTab always creates a pinned one', () => {
    const p = tabs().openTable('c-prod', orders, { preview: true });
    expect((tabs().tabs[p] as TableTab).preview).toBe(true);
    // Opened again as a preview: same tab, still a preview.
    expect(tabs().openTable('c-prod', orders, { preview: true })).toBe(p);
    expect((tabs().tabs[p] as TableTab).preview).toBe(true);
    // Opened pinned: same tab, now pinned.
    expect(tabs().openTable('c-prod', orders)).toBe(p);
    expect((tabs().tabs[p] as TableTab).preview).toBe(false);
    const n = tabs().openTable('c-prod', orders, { newTab: true, preview: true });
    expect(n).not.toBe(p);
    expect((tabs().tabs[n] as TableTab).preview).toBe(false);
    expect(ids()).toEqual([p, n]);
  });

  it('closes, reopens with the document, and activates the right neighbour', () => {
    const a = tabs().openQuery('c-prod');
    const b = tabs().openQuery('c-prod');
    const c = tabs().openQuery('c-prod');
    useExtractStore.getState().setSql(b, 'SELECT 1');
    tabs().activate(b);
    tabs().close(b);
    expect(ids()).toEqual([a, c]);
    expect(active()).toBe(c);
    expect(useExtractStore.getState().docs[b]).toBeUndefined();
    expect(tabs().reopenClosed('c-prod')).toBe(b);
    expect(ids()).toEqual([a, b, c]);
    expect(useExtractStore.getState().docs[b].sql).toBe('SELECT 1');
    expect(active()).toBe(b);
    expect(tabs().reopenClosed('c-prod')).toBeNull();
  });

  it('closes others / to the right / all, keeping at most 10 closed tabs', () => {
    const created = Array.from({ length: 12 }, () => tabs().openQuery('c-prod'));
    tabs().closeToRight(created[9]);
    expect(ids()).toHaveLength(10);
    tabs().closeOthers(created[0]);
    expect(ids()).toEqual([created[0]]);
    expect(active()).toBe(created[0]);
    tabs().closeAll('c-prod');
    expect(ids()).toEqual([]);
    expect(active()).toBeNull();
    expect(tabs().sets['c-prod'].closed).toHaveLength(10);
  });

  it('activates by index (-1 = last), cycles next / previous and reorders', () => {
    const [a, b, c] = [tabs().openQuery('c-prod'), tabs().openQuery('c-prod'), tabs().openQuery('c-prod')];
    tabs().activateIndex('c-prod', 0);
    expect(active()).toBe(a);
    tabs().activateIndex('c-prod', -1);
    expect(active()).toBe(c);
    tabs().activateNext('c-prod', 1);
    expect(active()).toBe(a);
    tabs().activateNext('c-prod', -1);
    expect(active()).toBe(c);
    tabs().reorder('c-prod', 2, 0);
    expect(ids()).toEqual([c, a, b]);
  });

  it('renames (empty = default) and pins a preview tab on rename', () => {
    const q = tabs().openQuery('c-prod');
    tabs().rename(q, '  daily  ');
    expect(tabTitle(tabs().tabs[q])).toBe('daily');
    tabs().rename(q, '');
    expect(tabs().tabs[q].customTitle).toBeUndefined();
    const p = tabs().openTable('c-prod', orders, { preview: true });
    tabs().rename(p, 'o');
    expect((tabs().tabs[p] as TableTab).preview).toBe(false);
  });

  it('keeps one history tab per workspace', () => {
    const h = tabs().openHistory('c-prod');
    tabs().openQuery('c-prod');
    expect(tabs().openHistory('c-prod')).toBe(h);
    expect(list().filter((t) => t.kind === 'history')).toHaveLength(1);
    expect(active()).toBe(h);
  });

  it('table Query view: pins, creates a doc filled with the select-all SQL, keeps it across toggles, disposes it with the tab', async () => {
    const t = tabs().openTable('c-prod', orders, { preview: true });
    expect(useExtractStore.getState().docs[t]).toBeUndefined();
    const pending = tabs().setTableView(t, 'query');
    expect(useExtractStore.getState().docs[t].busy).toBe('select');
    await pending;
    const doc = useExtractStore.getState().docs[t];
    expect(doc.sql).toMatch(/SELECT/i);
    expect(doc.name).toBe('orders');
    expect(isDocDirty(doc)).toBe(false);
    expect(doc.busy).toBeNull();
    expect((tabs().tabs[t] as TableTab).preview).toBe(false);
    expect(selectActiveQueryTarget(tabs(), 'c-prod')).toBe(t);

    useExtractStore.getState().setSql(t, 'SELECT id FROM public.orders');
    await tabs().setTableView(t, 'structure');
    expect(selectActiveQueryTarget(tabs(), 'c-prod')).toBeNull();
    await tabs().setTableView(t, 'query');
    expect(useExtractStore.getState().docs[t].sql).toBe('SELECT id FROM public.orders');

    tabs().close(t);
    expect(useExtractStore.getState().docs[t]).toBeUndefined();
  });

  it('refuses the Query view for a table outside the execution database (postgres)', async () => {
    const t = tabs().openTable('c-prod', { database: 'analytics', schema: 'public', name: 'events' });
    await tabs().setTableView(t, 'query');
    expect((tabs().tabs[t] as TableTab).view).toBe('structure');
    expect(useExtractStore.getState().docs[t]).toBeUndefined();
  });

  it('a select-all SQL arriving after the tab was closed and reopened never lands in it', async () => {
    const t = tabs().openTable('c-prod', orders);
    const pending = tabs().setTableView(t, 'query');
    tabs().close(t);
    tabs().reopenClosed('c-prod');
    await pending;
    // The reopened doc is the one closed while busy: restored idle, without the late result.
    const doc = useExtractStore.getState().docs[t];
    expect(doc.busy).toBeNull();
    expect(doc.sql).toBe('');
  });

  it('dropConnection disposes every tab and doc of that workspace only', () => {
    const p = tabs().openQuery('c-prod');
    const l = tabs().openQuery('c-local');
    tabs().dropConnection('c-prod');
    expect(tabs().sets['c-prod']).toBeUndefined();
    expect(useExtractStore.getState().docs[p]).toBeUndefined();
    expect(useExtractStore.getState().docs[l]).toBeDefined();
    expect(selectActiveTab(tabs(), 'c-local')?.id).toBe(l);
  });
});
