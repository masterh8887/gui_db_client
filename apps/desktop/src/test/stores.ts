// Test helper: snapshot every store's initial state and restore it between tests.

import { useCatalogStore } from '@/stores/catalog';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useHistoryStore } from '@/stores/history';
import { useJobsStore } from '@/stores/jobs';
import { useLogsStore } from '@/stores/logs';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';

const stores = [
  useCatalogStore,
  useConnectionsStore,
  useExtractStore,
  useHistoryStore,
  useJobsStore,
  useLogsStore,
  useSettingsStore,
  useUiStore,
] as const;

const initial = stores.map((s) => s.getState());

export function resetStores(): void {
  stores.forEach((s, i) => {
    (s.setState as (state: unknown, replace: true) => void)(initial[i], true);
  });
}
