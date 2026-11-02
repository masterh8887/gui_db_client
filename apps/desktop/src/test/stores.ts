// Test helper: snapshot every store's initial state and restore it between tests (tabs `nextId`
// and the workspace included). `src/test/setup.ts` also resets lib singletons after each test.

import { useConfirmQueue } from '@/lib/confirm';
import { useToastStore } from '@/lib/toast';
import { __resetCatalogForTests, useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { useLogsStore } from '@/stores/logs';
import { __resetSettingsForTests, useSettingsStore } from '@/stores/settings';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { resetWindowsInit, useWindowsStore } from '@/stores/windows';
import { __resetWorkspaceForTests, useWorkspaceStore } from '@/stores/workspace';

const stores = [
  useCatalogStore,
  useConnectionsStore,
  useCopyStore,
  useExtractStore,
  useHistoryStore,
  useJobsStore,
  useLogsStore,
  useSettingsStore,
  useTabsStore,
  useUiStore,
  useWorkspaceStore,
  useWindowsStore,
  useToastStore,
] as const;

const initial = stores.map((s) => s.getState());

export function resetStores(): void {
  stores.forEach((s, i) => {
    (s.setState as (state: unknown, replace: true) => void)(initial[i], true);
  });
  // Mounted ConfirmHosts stay registered; only queued requests are dropped.
  useConfirmQueue.setState({ queue: [] });
  __resetWorkspaceForTests();
  __resetCatalogForTests();
  __resetSettingsForTests();
  resetWindowsInit();
}
