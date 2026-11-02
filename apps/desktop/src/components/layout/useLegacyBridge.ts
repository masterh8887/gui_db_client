'use client';

// Interim bridge for the pre-redesign panels (removed in W3 together with the legacy store API):
// when old code asks for the extract form (`ui.setCenterTab('extract')`, e.g. "make extraction
// query" in the old tree or "load" in the old history tab), show the query tab whose document it
// edited, or open a new query tab with the private legacy document's content.

import { useEffect } from 'react';
import { LEGACY_DOC_ID, useExtractStore } from '@/stores/extract';
import { useTabsStore } from '@/stores/tabs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';

async function showLegacyDoc(): Promise<void> {
  const ex = useExtractStore.getState();
  const id = ex.legacyTabId;
  if (!id) return;
  const tabs = useTabsStore.getState();
  if (id !== LEGACY_DOC_ID) {
    const tab = tabs.tabs[id];
    if (!tab) return;
    const ws = useWorkspaceStore.getState();
    if (ws.mode !== 'workspace' || ws.openConnId !== tab.connId) await ws.openWorkspace(tab.connId);
    useTabsStore.getState().activate(id);
    return;
  }
  const doc = ex.docs[LEGACY_DOC_ID];
  if (!doc?.connId) return;
  const ws = useWorkspaceStore.getState();
  if (ws.mode !== 'workspace' || ws.openConnId !== doc.connId) {
    if (!(await ws.openWorkspace(doc.connId, { skipEmptyTab: true }))) return;
  }
  useTabsStore.getState().openQuery(doc.connId, {
    sql: doc.sql,
    sqlMode: doc.sqlMode,
    sqlFile: doc.sqlFile,
    name: doc.name,
    session: doc.session,
    output: doc.output,
  });
}

export function useLegacyBridge(): void {
  useEffect(() => {
    // Park the legacy flag so a later `setCenterTab('extract')` is a change we can see.
    useUiStore.setState({ centerTab: 'table' });
    return useUiStore.subscribe((s, prev) => {
      if (s.centerTab !== 'extract' || prev.centerTab === 'extract') return;
      useUiStore.setState({ centerTab: 'table' });
      void showLegacyDoc();
    });
  }, []);
}
