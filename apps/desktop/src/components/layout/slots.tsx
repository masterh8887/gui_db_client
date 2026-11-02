'use client';

// Slot seams whose current files still have the pre-redesign export (DESIGN.md §7.6). They
// resolve to the contract export as soon as its owner ships it, and to an adapter over the old
// export until then. W3: replace both with direct imports.

import { useEffect, useState, type ComponentType } from 'react';
import * as AboutModule from '@/components/dialogs/AboutDialog';
import * as ImportModule from '@/components/dialogs/ImportDbeaverDialog';
import { pickAnyFile } from '@/lib/dialogs';
import { errorText } from '@/lib/ipc-error';
import { useConnectionsStore } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

type AnyComponent = ComponentType<Record<string, unknown>>;
const aboutExports = AboutModule as unknown as Record<string, AnyComponent | undefined>;
const importExports = ImportModule as unknown as Record<string, AnyComponent | undefined>;

/** `AboutDialog()` reads `ui.aboutOpen`; the older one took `open` / `onOpenChange` (extra props are ignored by the new one). */
export function AboutSlot() {
  const open = useUiStore((s) => s.aboutOpen);
  const setOpen = useUiStore((s) => s.setAboutOpen);
  const About = aboutExports.AboutDialog;
  if (!About) return null;
  return <About open={open} onOpenChange={setOpen} />;
}

/** `ImportDbeaverFlow()` reads `ui.importDbeaverOpen`; until it exists, pick + parse here and show the older dialog. */
export function ImportDbeaverSlot() {
  const Flow = importExports.ImportDbeaverFlow;
  if (Flow) return <Flow />;
  return <LegacyImportAdapter Dialog={importExports.ImportDbeaverDialog} />;
}

interface ImportSource {
  path: string;
  rows: unknown[];
  error?: string;
}

function LegacyImportAdapter({ Dialog }: { Dialog: AnyComponent | undefined }) {
  const open = useUiStore((s) => s.importDbeaverOpen);
  const [source, setSource] = useState<ImportSource | null>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    void (async () => {
      const close = () => useUiStore.getState().setImportDbeaverOpen(false);
      let path: string | null;
      try {
        path = await pickAnyFile([{ name: 'DBeaver data-sources.json', extensions: ['json'] }]);
      } catch (e) {
        logLocal('error', `file dialog: ${errorText(e)}`);
        close();
        return;
      }
      if (cancelled) return;
      if (!path) {
        close();
        return;
      }
      try {
        const rows = await useConnectionsStore.getState().importDbeaver(path);
        if (!cancelled) setSource({ path, rows });
      } catch (e) {
        logLocal('error', `conn_import_dbeaver: ${errorText(e)}`);
        if (!cancelled) setSource({ path, rows: [], error: errorText(e) });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [open]);

  if (!open || !source || !Dialog) return null;
  return (
    <Dialog
      source={source}
      onClose={() => {
        setSource(null);
        useUiStore.getState().setImportDbeaverOpen(false);
      }}
    />
  );
}
