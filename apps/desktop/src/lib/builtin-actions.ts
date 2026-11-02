'use client';

// The built-in actions (DESIGN.md §7.5 list). Table actions target the active table tab, else
// the sidebar selection. Registered by `registerBuiltinActions()` in `./actions`.

import type { TableRef } from 'ipc-types';
import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore, type CopyPreset } from '@/stores/copy';
import { useExtractStore } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useLogsStore } from '@/stores/logs';
import { selectActiveQueryTarget, selectActiveTab, useTabsStore } from '@/stores/tabs';
import { useUiStore, type Theme } from '@/stores/ui';
import { useWindowsStore } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import type { AppAction } from './actions';
import {
  editConnection,
  newConnection,
  rescanDrivers,
  startImportDbeaver,
  testConnection,
} from './connection-actions';
import { pickSqlFile } from './dialogs';
import { askCancelLatest } from './job-actions';
import { isBusyState } from './job-state';
import { shortcutFor } from './keymap';
import { ls } from './strings';
import { canTargetTable, copyColumnList, copyDdl, countExactly, openExtractionQuery } from './table-actions';
import { tableLabel } from './utils';

/** The workspace on screen, or `null` on Welcome. */
function workspaceConn(): string | null {
  const w = useWorkspaceStore.getState();
  return w.mode === 'workspace' ? w.openConnId : null;
}

function activeTab() {
  return selectActiveTab(useTabsStore.getState(), workspaceConn());
}

function queryTarget(): string | null {
  return selectActiveQueryTarget(useTabsStore.getState(), workspaceConn());
}

/** Target of table actions: the active table tab, else the sidebar selection. */
export function tableActionTarget(): { connId: string; ref: TableRef } | null {
  const tab = activeTab();
  if (tab?.kind === 'table') return { connId: tab.connId, ref: tab.table };
  const connId = workspaceConn();
  const sel = connId ? useWorkspaceStore.getState().selection[connId] : null;
  return connId && sel ? { connId, ref: sel } : null;
}

/**
 * The copy wizard's source preset (`copy.open`, docs/db-copy.md 7): the workspace connection with
 * its current browse database and, on PostgreSQL, schema. MySQL has no schemas (its schema is
 * the database) and SQLite copies `main`. Welcome: nothing preset.
 */
export function copySourcePreset(): CopyPreset | undefined {
  const connId = workspaceConn();
  if (!connId) return undefined;
  const scope = useWorkspaceStore.getState().scope[connId];
  const driver = useConnectionsStore.getState().connections.find((c) => c.id === connId)?.driver;
  const database = driver === 'sqlite' ? undefined : (scope?.database ?? undefined);
  const schema = driver === 'postgres' && database ? (scope?.schema ?? undefined) : undefined;
  return { source: { connId, ...(database ? { database } : {}), ...(schema ? { schema } : {}) } };
}

/** Connection for connection actions: the workspace, else the Welcome selection. */
function connectionTarget(): string | null {
  return workspaceConn() ?? useConnectionsStore.getState().selectedId;
}

const inWorkspace = () => workspaceConn() !== null;
const hasTarget = () => queryTarget() !== null;

/**
 * True while the export job last started from query target `tabId` is pending / preflight /
 * running / finishing (per `job:*`). The query bar then shows 중지 instead of 실행 (DESIGN.md
 * §5.D), so the global Run must not start a second export from the same tab either.
 */
export function targetJobActive(tabId: string | null): boolean {
  if (!tabId) return false;
  const jobId = useExtractStore.getState().docs[tabId]?.lastJobId;
  const state = jobId ? useJobsStore.getState().jobs[jobId]?.summary.state : undefined;
  return state !== undefined && isBusyState(state);
}

const hasTable = () => tableActionTarget() !== null;
const tableName = () => {
  const t = tableActionTarget();
  return t ? tableLabel(t.ref) : '—';
};

export function builtinActions(): AppAction[] {
  const sc = (id: string) => shortcutFor(id);
  const tabSelect: AppAction[] = Array.from({ length: 8 }, (_, i) => ({
    id: `tabs.select${i + 1}`,
    group: 'tabs' as const,
    title: () => ls('tabs.select', { n: i + 1 }),
    shortcut: sc(`tabs.select${i + 1}`),
    hidden: true,
    enabled: inWorkspace,
    run: () => {
      const connId = workspaceConn();
      if (connId) useTabsStore.getState().activateIndex(connId, i);
    },
  }));
  const theme = (v: Theme): AppAction => ({
    id: `theme.${v}`,
    group: 'app',
    title: () => ls(`theme.${v}`),
    keywords: ['theme', '테마'],
    run: () => useUiStore.getState().setTheme(v),
  });

  return [
    // ---- palette, tabs ----------------------------------------------------------------
    {
      id: 'palette.open',
      group: 'app',
      title: () => ls('palette.open'),
      shortcut: sc('palette.open'),
      hidden: true,
      run: () => {
        const ui = useUiStore.getState();
        if (ui.palette.open) ui.closePalette();
        else ui.openPalette('all');
      },
    },
    {
      id: 'palette.connections',
      group: 'connection',
      title: () => ls('palette.connections'),
      shortcut: sc('palette.connections'),
      run: () => useUiStore.getState().openPalette('connections'),
    },
    {
      id: 'tabs.newQuery',
      group: 'tabs',
      title: () => ls('tabs.newQuery'),
      shortcut: sc('tabs.newQuery'),
      enabled: inWorkspace,
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().openQuery(connId);
      },
    },
    {
      id: 'tabs.close',
      group: 'tabs',
      title: () => ls('tabs.close'),
      shortcut: sc('tabs.close'),
      enabled: () => !!activeTab(),
      run: () => {
        const tab = activeTab();
        if (tab) useTabsStore.getState().close(tab.id);
      },
    },
    {
      id: 'tabs.reopenClosed',
      group: 'tabs',
      title: () => ls('tabs.reopenClosed'),
      shortcut: sc('tabs.reopenClosed'),
      enabled: () => {
        const connId = workspaceConn();
        return !!connId && (useTabsStore.getState().sets[connId]?.closed.length ?? 0) > 0;
      },
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().reopenClosed(connId);
      },
    },
    {
      id: 'tabs.openHistory',
      group: 'tabs',
      title: () => ls('tabs.openHistory'),
      keywords: ['history', '기록'],
      enabled: inWorkspace,
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().openHistory(connId);
      },
    },
    ...tabSelect,
    {
      id: 'tabs.selectLast',
      group: 'tabs',
      title: () => ls('tabs.selectLast'),
      shortcut: sc('tabs.selectLast'),
      hidden: true,
      enabled: inWorkspace,
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().activateIndex(connId, -1);
      },
    },
    {
      id: 'tabs.next',
      group: 'tabs',
      title: () => ls('tabs.next'),
      shortcut: sc('tabs.next'),
      enabled: inWorkspace,
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().activateNext(connId, 1);
      },
    },
    {
      id: 'tabs.previous',
      group: 'tabs',
      title: () => ls('tabs.previous'),
      shortcut: sc('tabs.previous'),
      enabled: inWorkspace,
      run: () => {
        const connId = workspaceConn();
        if (connId) useTabsStore.getState().activateNext(connId, -1);
      },
    },

    // ---- query ------------------------------------------------------------------------
    {
      id: 'query.run',
      group: 'query',
      title: () => ls('query.run'),
      shortcut: sc('query.run'),
      keywords: ['run', 'export', '실행', '추출'],
      enabled: () => inWorkspace() && !targetJobActive(queryTarget()),
      run: async () => {
        const connId = workspaceConn();
        if (!connId) return;
        // Rust is the state of record: while this tab's job is active Run is a no-op (the bar's
        // 중지 asks to cancel instead).
        if (targetJobActive(queryTarget())) return;
        const tab = activeTab();
        if (!tab || tab.kind === 'history') {
          useTabsStore.getState().openQuery(connId);
          return;
        }
        if (tab.kind === 'table' && tab.view === 'structure') {
          // A second Mod+Enter runs; no-op when 쿼리 is disabled (setTableView refuses).
          await useTabsStore.getState().setTableView(tab.id, 'query');
          return;
        }
        await useExtractStore.getState().run(tab.id);
      },
    },
    {
      id: 'query.explain',
      group: 'query',
      title: () => ls('query.explain'),
      shortcut: sc('query.explain'),
      keywords: ['explain', 'plan', '계획'],
      enabled: hasTarget,
      run: async () => {
        const id = queryTarget();
        if (id) await useExtractStore.getState().explain(id);
      },
    },
    {
      id: 'query.check',
      group: 'query',
      title: () => ls('query.check'),
      shortcut: sc('query.check'),
      keywords: ['check', 'validate', '검사'],
      enabled: hasTarget,
      run: async () => {
        const id = queryTarget();
        if (id) await useExtractStore.getState().check(id);
      },
    },
    {
      id: 'query.openFile',
      group: 'query',
      title: () => ls('query.openFile'),
      shortcut: sc('query.openFile'),
      keywords: ['sql', 'file', '파일'],
      enabled: inWorkspace,
      run: async () => {
        const connId = workspaceConn();
        if (!connId) return;
        const path = await pickSqlFile();
        if (!path) return;
        const id = queryTarget() ?? useTabsStore.getState().openQuery(connId);
        await useExtractStore.getState().loadSqlFile(id, path);
      },
    },
    {
      id: 'query.excelPreset',
      group: 'query',
      title: () => ls('query.excelPreset'),
      keywords: ['excel', 'csv', 'bom', '엑셀'],
      enabled: hasTarget,
      run: () => {
        const id = queryTarget();
        if (!id) return;
        useExtractStore.getState().applyExcelPreset(id);
        useExtractStore.getState().setPanel(id, 'output');
      },
    },
    {
      id: 'query.openInConnection',
      group: 'query',
      title: () => ls('query.openInConnection'),
      enabled: hasTarget,
      run: () => {
        const id = queryTarget();
        if (id) useUiStore.getState().openPalette('connections', '', { kind: 'openInConnection', tabId: id });
      },
    },

    // ---- view -------------------------------------------------------------------------
    {
      id: 'view.toggleSidebar',
      group: 'view',
      title: () => ls('view.toggleSidebar'),
      shortcut: sc('view.toggleSidebar'),
      enabled: inWorkspace,
      run: () => useUiStore.getState().toggleSidebar(),
    },
    {
      id: 'view.toggleInspector',
      group: 'view',
      title: () => ls('view.toggleInspector'),
      shortcut: sc('view.toggleInspector'),
      enabled: inWorkspace,
      run: () => {
        const ui = useUiStore.getState();
        if (!ui.inspectorOpen && activeTab()?.kind === 'table') ui.setInspectorSegment('details');
        ui.toggleInspector();
      },
    },
    {
      id: 'view.toggleConsole',
      group: 'view',
      title: () => ls('view.toggleConsole'),
      shortcut: sc('view.toggleConsole'),
      keywords: ['log', 'console', '로그', '콘솔'],
      run: () => {
        // Opening or closing both acknowledge the error flag (the user has looked).
        useLogsStore.getState().ackError();
        useUiStore.getState().toggleLog();
      },
    },
    {
      id: 'view.toggleExportPanel',
      group: 'view',
      title: () => ls('view.toggleExportPanel'),
      shortcut: sc('view.toggleExportPanel'),
      enabled: hasTarget,
      run: () => {
        const id = queryTarget();
        if (id) useExtractStore.getState().togglePanel(id);
      },
    },

    // ---- catalog ----------------------------------------------------------------------
    {
      id: 'catalog.refresh',
      group: 'catalog',
      title: () => ls('catalog.refresh'),
      shortcut: sc('catalog.refresh'),
      keywords: ['reload', 'refresh', '새로 고침'],
      run: async () => {
        const connId = workspaceConn();
        if (!connId) {
          await useConnectionsStore.getState().load();
          return;
        }
        await useCatalogStore.getState().refresh(connId);
        await useWorkspaceStore.getState().ensureScope(connId);
      },
    },
    {
      id: 'catalog.switchDatabase',
      group: 'catalog',
      title: () => ls('catalog.switchDatabase'),
      shortcut: sc('catalog.switchDatabase'),
      keywords: ['database', '데이터베이스'],
      enabled: inWorkspace,
      run: () => useUiStore.getState().requestSidebar('database'),
    },
    {
      id: 'catalog.switchSchema',
      group: 'catalog',
      title: () => ls('catalog.switchSchema'),
      keywords: ['schema', '스키마'],
      enabled: inWorkspace,
      run: () => useUiStore.getState().requestSidebar('schema'),
    },
    {
      id: 'sidebar.focusSearch',
      group: 'catalog',
      title: () => ls('sidebar.focusSearch'),
      shortcut: sc('sidebar.focusSearch'),
      enabled: inWorkspace,
      run: () => useUiStore.getState().requestSidebar('focusSearch'),
    },

    // ---- table (active table tab, else sidebar selection) -------------------------------
    {
      id: 'table.countExactly',
      group: 'catalog',
      title: () => ls('table.countExactly', { table: tableName() }),
      keywords: ['count', '세기', '행 수'],
      enabled: hasTable,
      run: async () => {
        const t = tableActionTarget();
        if (t) await countExactly(t.connId, t.ref);
      },
    },
    {
      id: 'table.copyColumns',
      group: 'catalog',
      title: () => ls('table.copyColumns', { table: tableName() }),
      enabled: hasTable,
      run: async () => {
        const t = tableActionTarget();
        if (t) await copyColumnList(t.connId, t.ref);
      },
    },
    {
      id: 'table.copyDdl',
      group: 'catalog',
      title: () => ls('table.copyDdl', { table: tableName() }),
      enabled: hasTable,
      run: async () => {
        const t = tableActionTarget();
        if (t) await copyDdl(t.connId, t.ref);
      },
    },
    {
      id: 'table.makeQuery',
      group: 'catalog',
      title: () => ls('table.makeQuery', { table: tableName() }),
      keywords: ['select', 'query', '쿼리'],
      enabled: () => {
        const t = tableActionTarget();
        return !!t && canTargetTable(t.connId, t.ref).ok;
      },
      run: async () => {
        const t = tableActionTarget();
        if (t) await openExtractionQuery(t.connId, t.ref);
      },
    },
    {
      id: 'table.showStructure',
      group: 'catalog',
      title: () => ls('table.showStructure', { table: tableName() }),
      enabled: hasTable,
      run: async () => {
        const t = tableActionTarget();
        if (!t) return;
        const id = useTabsStore.getState().openTable(t.connId, t.ref, { preview: false });
        await useTabsStore.getState().setTableView(id, 'structure');
      },
    },
    {
      id: 'table.showQuery',
      group: 'catalog',
      title: () => ls('table.showQuery', { table: tableName() }),
      enabled: () => {
        const t = tableActionTarget();
        return !!t && canTargetTable(t.connId, t.ref).ok;
      },
      run: async () => {
        const t = tableActionTarget();
        if (t) await openExtractionQuery(t.connId, t.ref);
      },
    },

    // ---- connections ------------------------------------------------------------------
    {
      id: 'connection.new',
      group: 'connection',
      title: () => ls('connection.new'),
      shortcut: sc('connection.new'),
      run: () => newConnection(),
    },
    {
      id: 'connection.edit',
      group: 'connection',
      title: () => ls('connection.edit'),
      enabled: () => connectionTarget() !== null,
      run: () => {
        const id = connectionTarget();
        if (id) editConnection(id);
      },
    },
    {
      id: 'connection.test',
      group: 'connection',
      title: () => ls('connection.test'),
      enabled: () => connectionTarget() !== null,
      run: async () => {
        const id = connectionTarget();
        if (id) await testConnection(id);
      },
    },
    {
      id: 'connection.importDbeaver',
      group: 'connection',
      title: () => ls('connection.importDbeaver'),
      keywords: ['dbeaver', 'import', '가져오기'],
      run: () => startImportDbeaver(),
    },
    {
      // DB copy (docs/db-copy.md 7): in a workspace the source starts as this connection and the
      // database (PostgreSQL: and schema) the sidebar browses.
      id: 'copy.open',
      group: 'connection',
      title: () => ls('copy.open'),
      keywords: ['copy', 'migrate', 'migration', '복사', '이전', '마이그레이션'],
      run: () => useCopyStore.getState().open(copySourcePreset()),
    },
    {
      id: 'window.new',
      group: 'app',
      title: () => ls('window.new'),
      shortcut: sc('window.new'),
      keywords: ['window', '창'],
      run: () => useWindowsStore.getState().openNew(),
    },
    {
      // Welcome row / palette connection row: Mod+Enter there (their own bindings).
      id: 'window.openConnection',
      group: 'connection',
      title: () => ls('window.openConnection'),
      keywords: ['window', '창'],
      enabled: () => connectionTarget() !== null,
      run: async () => {
        const id = connectionTarget();
        if (id) await useWindowsStore.getState().openNew(id);
      },
    },
    {
      id: 'app.newInstance',
      group: 'app',
      title: () => ls('app.newInstance'),
      keywords: ['instance', 'process', '실행'],
      run: () => useWindowsStore.getState().spawnInstance(),
    },
    {
      id: 'drivers.rescan',
      group: 'connection',
      title: () => ls('drivers.rescan'),
      keywords: ['driver', 'oracle', 'odbc', '드라이버'],
      run: async () => {
        await rescanDrivers();
      },
    },

    // ---- app --------------------------------------------------------------------------
    {
      id: 'app.welcome',
      group: 'app',
      title: () => ls('app.welcome'),
      enabled: inWorkspace,
      run: () => useWorkspaceStore.getState().showWelcome(),
    },
    {
      id: 'app.closeWorkspace',
      group: 'app',
      title: () => ls('app.closeWorkspace'),
      enabled: inWorkspace,
      run: async () => {
        const connId = workspaceConn();
        if (connId) await useWorkspaceStore.getState().closeWorkspace(connId);
      },
    },
    {
      id: 'app.settings',
      group: 'app',
      title: () => ls('app.settings'),
      shortcut: sc('app.settings'),
      keywords: ['settings', 'preferences', '설정'],
      run: () => useUiStore.getState().setSettingsOpen(true),
    },
    {
      id: 'app.about',
      group: 'app',
      title: () => ls('app.about'),
      keywords: ['about', 'version', '버전'],
      run: () => useUiStore.getState().setAboutOpen(true),
    },
    {
      id: 'app.diagnostics',
      group: 'app',
      title: () => ls('app.diagnostics'),
      keywords: ['diagnostics', 'bundle', '진단'],
      run: () => useUiStore.getState().setAboutOpen(true),
    },
    theme('system'),
    theme('light'),
    theme('dark'),
    {
      id: 'lang.ko',
      group: 'app',
      title: () => ls('lang.ko'),
      keywords: ['language', 'korean', '언어'],
      run: () => useUiStore.getState().setLang('ko'),
    },
    {
      id: 'lang.en',
      group: 'app',
      title: () => ls('lang.en'),
      keywords: ['language', 'english', '언어'],
      run: () => useUiStore.getState().setLang('en'),
    },

    // ---- jobs, log --------------------------------------------------------------------
    {
      id: 'jobs.cancelAsk',
      group: 'jobs',
      title: () => ls('jobs.cancelAsk'),
      shortcut: 'mod+.',
      keywords: ['cancel', 'stop', '취소', '중지'],
      run: () => askCancelLatest(),
    },
    {
      id: 'jobs.clearFinished',
      group: 'jobs',
      title: () => ls('jobs.clearFinished'),
      run: () => useJobsStore.getState().clearFinished(),
    },
    {
      id: 'log.clear',
      group: 'app',
      title: () => ls('log.clear'),
      run: () => useLogsStore.getState().clear(),
    },
  ];
}
