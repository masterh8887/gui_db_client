import type { JobState, JobSummary } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { LEGACY_CONN_ID, mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useExtractStore } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useLogsStore } from '@/stores/logs';
import { selectTabList, useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { resetStores } from '@/test/stores';
import { getAction, getActions, registerActions, registerBuiltinActions, runAction } from './actions';
import { copySourcePreset } from './builtin-actions';
import { useToastStore } from './toast';

const BUILTIN_IDS = [
  'palette.open', 'palette.connections', 'tabs.newQuery', 'tabs.close', 'tabs.reopenClosed', 'tabs.openHistory', 'query.run',
  'query.explain', 'query.check', 'query.openFile', 'query.excelPreset', 'query.openInConnection', 'view.toggleSidebar',
  'view.toggleInspector', 'view.toggleConsole', 'view.toggleExportPanel', 'catalog.refresh', 'catalog.switchDatabase',
  'catalog.switchSchema', 'table.countExactly', 'table.copyColumns', 'table.copyDdl', 'table.makeQuery', 'table.showStructure',
  'table.showQuery', 'connection.new', 'connection.edit', 'connection.test', 'connection.importDbeaver', 'drivers.rescan',
  'app.welcome', 'app.closeWorkspace', 'app.settings', 'app.about', 'app.diagnostics', 'theme.system', 'theme.light',
  'theme.dark', 'lang.ko', 'lang.en', 'jobs.cancelAsk', 'jobs.clearFinished', 'log.clear', 'sidebar.focusSearch',
];

describe('actions', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('registers every built-in id once (idempotent)', () => {
    registerBuiltinActions();
    const count = getActions().length;
    registerBuiltinActions();
    expect(getActions()).toHaveLength(count);
    for (const id of BUILTIN_IDS) expect(getAction(id), id).toBeDefined();
  });

  it('runAction skips disabled and unknown actions and turns failures into a log line + toast', async () => {
    const run = vi.fn();
    const off = registerActions([
      { id: 't.off', group: 'app', title: () => 'Off', enabled: () => false, run },
      { id: 't.boom', group: 'app', title: () => 'Boom', run: () => Promise.reject(new Error('kaput')) },
    ]);
    await runAction('t.off');
    expect(run).not.toHaveBeenCalled();
    await runAction('t.nope');
    expect(useLogsStore.getState().entries.at(-1)).toMatchObject({ level: 'warn' });
    await expect(runAction('t.boom')).resolves.toBeUndefined();
    expect(useLogsStore.getState().entries.at(-1)).toMatchObject({ level: 'error' });
    expect(useLogsStore.getState().entries.at(-1)?.message).toMatch(/Boom.*kaput/);
    expect(useToastStore.getState().toasts.at(-1)).toMatchObject({ tone: 'danger' });
    off();
    expect(getAction('t.boom')).toBeUndefined();
  });

  it('query.run: no tab → a new query tab; table tab in Structure → its Query view', async () => {
    registerBuiltinActions();
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    useTabsStore.getState().closeAll('c-prod');
    await runAction('query.run');
    expect(selectTabList(useTabsStore.getState(), 'c-prod').map((t) => t.kind)).toEqual(['query']);
    const t = useTabsStore.getState().openTable('c-prod', { database: 'orders', schema: 'public', name: 'orders' });
    await runAction('query.run');
    expect(useTabsStore.getState().tabs[t]).toMatchObject({ view: 'query' });
  });

  it('query.run is a no-op while the tab job is pending / preflight / running / finishing (the bar shows 중지)', async () => {
    registerBuiltinActions();
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    const tabId = useTabsStore.getState().openQuery('c-prod', { sql: 'select 1' });
    useExtractStore.setState((s) => ({ docs: { ...s.docs, [tabId]: { ...s.docs[tabId], lastJobId: 'j1' } } }));
    const summary = (state: JobState): JobSummary => ({
      id: 'j1', kind: 'export', state, connId: 'c-prod', connName: 'prod', label: 'j1', startedAt: '2026-10-27T00:00:00Z',
    });
    for (const state of ['pending', 'preflight', 'running', 'finishing'] as const) {
      useJobsStore.getState().upsertSummary(summary(state));
      mockBackend.calls.length = 0;
      expect(getAction('query.run')?.enabled?.(), state).toBe(false);
      await getAction('query.run')?.run();
      expect(mockBackend.calls, state).not.toContain('export_preflight');
      expect(useExtractStore.getState().docs[tabId].busy, state).toBeNull();
    }
    // Once Rust reports a terminal state the same tab runs again.
    useJobsStore.getState().upsertSummary(summary('done'));
    expect(getAction('query.run')?.enabled?.()).toBe(true);
    await runAction('query.run');
    expect(mockBackend.calls).toContain('export_preflight');
  });

  it('palette.open toggles; welcome-only and workspace-only actions follow the mode', async () => {
    registerBuiltinActions();
    await runAction('palette.open');
    expect(useUiStore.getState().palette.open).toBe(true);
    await runAction('palette.open');
    expect(useUiStore.getState().palette.open).toBe(false);
    expect(getAction('tabs.newQuery')?.enabled?.()).toBe(false);
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    expect(getAction('tabs.newQuery')?.enabled?.()).toBe(true);
    await runAction('view.toggleInspector');
    expect(useUiStore.getState().inspectorOpen).toBe(true);
    await runAction('app.welcome');
    expect(useWorkspaceStore.getState().mode).toBe('welcome');
  });

  it('view.toggleConsole acknowledges the error flag when it opens and when it closes', async () => {
    registerBuiltinActions();
    useLogsStore.setState({ errorFlag: true });
    await runAction('view.toggleConsole');
    expect(useUiStore.getState().logOpen).toBe(true);
    expect(useLogsStore.getState().errorFlag).toBe(false);
    // An error auto-opened (or arrived in) the open console: closing it acks too.
    useLogsStore.setState({ errorFlag: true });
    await runAction('view.toggleConsole');
    expect(useUiStore.getState().logOpen).toBe(false);
    expect(useLogsStore.getState().errorFlag).toBe(false);
  });

  it('catalog.refresh invalidates the backend cache before reloading (workspace); reloads connections on Welcome', async () => {
    registerBuiltinActions();
    await runAction('catalog.refresh');
    expect(mockBackend.calls).toContain('conn_list');
    await useWorkspaceStore.getState().openWorkspace('c-prod');
    mockBackend.calls.length = 0;
    await runAction('catalog.refresh');
    const inv = mockBackend.calls.indexOf('catalog_invalidate');
    expect(inv).toBeGreaterThanOrEqual(0);
    expect(mockBackend.calls.indexOf('catalog_databases')).toBeGreaterThan(inv);
  });
  it('copy.open presets the workspace connection and its browse database / schema (Welcome: nothing)', async () => {
    registerBuiltinActions();
    await runAction('copy.open');
    expect(useCopyStore.getState().wizard.draft.source).toEqual({ connId: '', database: '' });
    useWorkspaceStore.setState({ mode: 'workspace', openConnId: 'c-prod', opened: ['c-prod'], scope: { 'c-prod': { database: 'analytics_dw', schema: 'staging' } } });
    await runAction('copy.open');
    expect(useCopyStore.getState().wizard).toMatchObject({ open: true, step: 'endpoints' });
    expect(useCopyStore.getState().wizard.draft.source).toEqual({ connId: 'c-prod', database: 'analytics_dw', schema: 'staging' });
    expect(copySourcePreset()).toEqual({ source: { connId: 'c-prod', database: 'analytics_dw', schema: 'staging' } });
    // MySQL: the database only (its schema is the database); SQLite: main.
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()] });
    useWorkspaceStore.setState({ openConnId: LEGACY_CONN_ID, scope: { [LEGACY_CONN_ID]: { database: 'board', schema: 'board' } } });
    expect(copySourcePreset()).toEqual({ source: { connId: LEGACY_CONN_ID, database: 'board' } });
    useWorkspaceStore.setState({ openConnId: 'c-local', scope: { 'c-local': { database: 'main', schema: 'main' } } });
    await runAction('copy.open');
    expect(useCopyStore.getState().wizard.draft.source).toEqual({ connId: 'c-local', database: 'main' });
  });
});
