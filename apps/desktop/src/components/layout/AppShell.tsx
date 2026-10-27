'use client';

// Three-pane desktop layout: tree | center tabs | monitor, with the collapsible log at the
// bottom and every dialog mounted once here.

import { useEffect } from 'react';
import { useGlobalShortcuts } from '@/lib/shortcuts';
import { AppEvents } from '@/components/AppEvents';
import { ConnectionDialog } from '@/components/dialogs/ConnectionDialog';
import { DbPasswordDialog } from '@/components/dialogs/DbPasswordDialog';
import { HostKeyDialog } from '@/components/dialogs/HostKeyDialog';
import { SettingsDialog } from '@/components/dialogs/SettingsDialog';
import { SshPasswordDialog } from '@/components/dialogs/SshPasswordDialog';
import { PreflightDialog } from '@/components/extract/PreflightDialog';
import { LogPanel } from '@/components/log/LogPanel';
import { MonitorPanel } from '@/components/monitor/MonitorPanel';
import { ConnectionTree } from '@/components/tree/ConnectionTree';
import { SplitPane, TooltipProvider } from '@/components/ui';
import { useExtractStore } from '@/stores/extract';
import { selectActiveJobs, useJobsStore } from '@/stores/jobs';
import { LAYOUT_LIMITS, applyTheme, rehydrateUi, useUiStore } from '@/stores/ui';
import { CenterTabs } from './CenterTabs';
import { HeaderBar } from './HeaderBar';

export function AppShell() {
  const theme = useUiStore((s) => s.theme);
  const lang = useUiStore((s) => s.lang);
  const leftWidth = useUiStore((s) => s.leftWidth);
  const rightWidth = useUiStore((s) => s.rightWidth);
  const logHeight = useUiStore((s) => s.logHeight);
  const logOpen = useUiStore((s) => s.logOpen);
  const setLeftWidth = useUiStore((s) => s.setLeftWidth);
  const setRightWidth = useUiStore((s) => s.setRightWidth);
  const setLogHeight = useUiStore((s) => s.setLogHeight);

  // Persisted panel sizes / theme / language load after the first (prerender-identical) render.
  useEffect(() => {
    void rehydrateUi();
  }, []);
  useEffect(() => applyTheme(theme), [theme]);

  useGlobalShortcuts([
    {
      key: 'Enter',
      mod: true,
      handler: () => {
        useUiStore.getState().setCenterTab('extract');
        void useExtractStore.getState().run();
      },
    },
    {
      key: 'e',
      mod: true,
      handler: () => {
        useUiStore.getState().setCenterTab('extract');
        void useExtractStore.getState().explain();
      },
    },
    {
      // Asks first (the monitor card shows the confirmation); a second Esc dismisses it.
      key: 'Escape',
      inInputs: true,
      handler: () => {
        const ui = useUiStore.getState();
        if (ui.cancelConfirmJobId) {
          ui.setCancelConfirm(null);
          return;
        }
        const active = selectActiveJobs(useJobsStore.getState());
        if (active.length > 0) ui.setCancelConfirm(active[active.length - 1].summary.id);
      },
    },
  ]);

  return (
    <TooltipProvider>
      <AppEvents />
      {/* Keyed by language so a change re-renders every translated string. */}
      <div key={lang} className="flex h-full w-full flex-col bg-background text-foreground">
        <HeaderBar />
        <div className="min-h-0 flex-1">
          <SplitPane
            direction="vertical"
            sized="second"
            size={logOpen ? logHeight : LAYOUT_LIMITS.logCollapsed}
            min={LAYOUT_LIMITS.logMin}
            max={LAYOUT_LIMITS.logMax}
            disabled={!logOpen}
            onResize={setLogHeight}
          >
            <SplitPane
              direction="horizontal"
              sized="first"
              size={leftWidth}
              min={LAYOUT_LIMITS.leftMin}
              max={LAYOUT_LIMITS.leftMax}
              onResize={setLeftWidth}
            >
              <ConnectionTree />
              <SplitPane
                direction="horizontal"
                sized="second"
                size={rightWidth}
                min={LAYOUT_LIMITS.rightMin}
                max={LAYOUT_LIMITS.rightMax}
                onResize={setRightWidth}
              >
                <CenterTabs />
                <MonitorPanel />
              </SplitPane>
            </SplitPane>
            <LogPanel />
          </SplitPane>
        </div>
        <ConnectionDialog />
        <SettingsDialog />
        <PreflightDialog />
        <DbPasswordDialog />
        <HostKeyDialog />
        <SshPasswordDialog />
      </div>
    </TooltipProvider>
  );
}
