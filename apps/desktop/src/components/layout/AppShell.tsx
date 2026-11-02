'use client';

// The app frame (DESIGN.md §4): Welcome (no workspace open) or one connection's workspace, the
// console drawer along the bottom, and every dialog / sheet / host mounted once. Sets
// `data-platform`, `<html lang>`, `--sheet-top`, the context-menu guard, the WebView
// accelerator guard and registers the built-in actions.

import { useEffect } from 'react';
import { AppEvents } from '@/components/AppEvents';
import { CopyWizard } from '@/components/copy/CopyWizard';
import { ConnectionDialog } from '@/components/dialogs/ConnectionDialog';
import { DbPasswordDialog } from '@/components/dialogs/DbPasswordDialog';
import { HostKeyDialog } from '@/components/dialogs/HostKeyDialog';
import { SettingsDialog } from '@/components/dialogs/SettingsDialog';
import { SshPasswordDialog } from '@/components/dialogs/SshPasswordDialog';
import { LogStatusButton } from '@/components/log/LogStatusButton';
import { JobChip } from '@/components/monitor/JobChip';
import { CommandPalette } from '@/components/palette/CommandPalette';
import { PreflightSheet } from '@/components/query/PreflightSheet';
import { ConfirmHost, Toaster, TooltipProvider } from '@/components/ui';
import { WelcomeScreen } from '@/components/welcome/WelcomeScreen';
import { registerBuiltinActions } from '@/lib/actions';
import { inTauri } from '@/lib/ipc';
import { applyPlatform, isMac, useIsMac } from '@/lib/platform';
import { isEditable, useWebViewAcceleratorGuard } from '@/lib/shortcuts';
import { applyTheme, rehydrateUi, useUiStore } from '@/stores/ui';
import { initWindows } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { ConsoleFrame } from './ConsoleFrame';
import { AboutSlot, ImportDbeaverSlot } from './slots';
import { useLegacyBridge } from './useLegacyBridge';
import { useWorkspaceShortcuts } from './useWorkspaceShortcuts';
import { Workspace } from './Workspace';

/**
 * Block the WebView's own context menu (Reload, Inspect…) unless the target is editable, inside
 * selected text, or inside a component with its own ContextMenu.
 */
function useContextMenuGuard(): void {
  useEffect(() => {
    const onMenu = (e: MouseEvent) => {
      if (e.defaultPrevented) return;
      const t = e.target;
      if (!(t instanceof Element)) return;
      if (isEditable(t) || t.closest('input, textarea, .cm-editor, [contenteditable="true"], [data-own-context-menu]')) return;
      const sel = window.getSelection();
      if (sel && !sel.isCollapsed && sel.containsNode(t, true)) return;
      e.preventDefault();
    };
    window.addEventListener('contextmenu', onMenu);
    return () => window.removeEventListener('contextmenu', onMenu);
  }, []);
}

/** Inside Tauri on macOS the native menu owns ⌘R / ⌘P (the accelerator guard leaves them alone). */
const nativeMenuOwnsModKeys = () => inTauri() && isMac();

/** Welcome: the toolbar-grey backdrop (macOS: a 32px drag strip under the traffic lights). */
function WelcomeBackdrop() {
  const mac = useIsMac();
  return (
    <div className="relative flex h-full items-center justify-center bg-toolbar">
      {mac && <div data-tauri-drag-region className="absolute inset-x-0 top-0 h-8" />}
      <WelcomeScreen
        headerEnd={
          <>
            <JobChip variant="welcome" />
            <LogStatusButton />
          </>
        }
      />
    </div>
  );
}

export function AppShell() {
  const theme = useUiStore((s) => s.theme);
  const lang = useUiStore((s) => s.lang);
  const logOpen = useUiStore((s) => s.logOpen);
  const mode = useWorkspaceStore((s) => s.mode);
  const openConnId = useWorkspaceStore((s) => s.openConnId);
  const inWorkspace = mode === 'workspace' && !!openConnId;

  // Persisted UI state loads after the first (prerender-identical) render.
  useEffect(() => {
    applyPlatform();
    registerBuiltinActions();
    void rehydrateUi();
    // Who this window is; a window opened for a connection opens its workspace (db-copy.md 1.1).
    void initWindows();
  }, []);
  useEffect(() => applyTheme(theme), [theme]);
  // Screen-reader voice and CJK line breaking follow the UI language (prerendered as `ko`).
  useEffect(() => {
    document.documentElement.lang = lang;
  }, [lang]);
  useEffect(() => {
    document.documentElement.style.setProperty('--sheet-top', inWorkspace ? '48px' : '16px');
  }, [inWorkspace]);
  useContextMenuGuard();
  useWebViewAcceleratorGuard(nativeMenuOwnsModKeys);
  useWorkspaceShortcuts(inWorkspace ? 'workspace' : 'welcome', inWorkspace ? openConnId : null);
  useLegacyBridge();

  return (
    <TooltipProvider>
      {/* Outside the key={lang} subtree: never remounted, never re-subscribes. */}
      <AppEvents />
      <div key={lang} className="flex h-full w-full flex-col bg-window text-fg">
        <div className="min-h-0 flex-1">{inWorkspace ? <Workspace connId={openConnId} /> : <WelcomeBackdrop />}</div>
        {logOpen && <ConsoleFrame />}
        <CommandPalette />
        <ConnectionDialog />
        <ImportDbeaverSlot />
        <SettingsDialog />
        <AboutSlot />
        <PreflightSheet />
        <CopyWizard />
        <DbPasswordDialog />
        <HostKeyDialog />
        <SshPasswordDialog />
        <ConfirmHost />
        <Toaster />
      </div>
    </TooltipProvider>
  );
}
