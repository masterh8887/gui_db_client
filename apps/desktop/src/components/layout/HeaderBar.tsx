'use client';

// LEGACY (deleted in W3): the pre-redesign header. Not mounted any more; kept because the old
// connection tree imports `StatusDot` from here until the sidebar owner replaces it.

import { Database, Info, Monitor, Moon, Plus, Settings, Sun } from 'lucide-react';
import type { ComponentType } from 'react';
import * as AboutModule from '@/components/dialogs/AboutDialog';
import { getLang, t } from '@/lib/i18n';
import { inTauri } from '@/lib/ipc';
import { Badge, DropdownMenu, IconButton, Select, StatusDot } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useUiStore, type Theme } from '@/stores/ui';

/** Moved to `@/components/ui` (DESIGN.md §7.4); re-exported for older imports. */
export { StatusDot };

const themeIcon: Record<Theme, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };

/** Label of the About button (not in `lib/i18n`, kept with the header). */
const ABOUT_LABEL = { ko: 'BulkDB 정보', en: 'About BulkDB' } as const;

const AboutDialog = (AboutModule as unknown as Record<string, ComponentType<{ open?: boolean; onOpenChange?: (o: boolean) => void }>>)
  .AboutDialog;

export function HeaderBar() {
  const connections = useConnectionsStore((s) => s.connections);
  const selectedId = useConnectionsStore((s) => s.selectedId);
  const status = useConnectionsStore((s) => (s.selectedId ? (s.status[s.selectedId] ?? 'unknown') : 'unknown'));
  const select = useConnectionsStore((s) => s.select);
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);
  const openConnectionDialog = useUiStore((s) => s.openConnectionDialog);
  const setSettingsOpen = useUiStore((s) => s.setSettingsOpen);
  const aboutOpen = useUiStore((s) => s.aboutOpen);
  const setAboutOpen = useUiStore((s) => s.setAboutOpen);
  const ThemeIcon = themeIcon[theme];

  const onSelect = (id: string) => {
    select(id);
    useExtractStore.getState().setConnId(id);
  };

  return (
    <header className="flex h-9 shrink-0 items-center gap-2 border-b border-separator bg-toolbar px-2">
      <div className="flex items-center gap-1.5 pr-2">
        <Database size={16} className="text-accent" />
        <span className="text-base font-semibold">{t('app.name')}</span>
        {!inTauri() && (
          <Badge tone="warning" title="Running against the in-browser mock backend">
            mock
          </Badge>
        )}
      </div>
      <div className="flex items-center gap-2">
        <StatusDot status={status} />
        <div className="w-56">
          <Select
            aria-label={t('header.connection')}
            value={selectedId ?? undefined}
            onChange={onSelect}
            placeholder={t('header.noConnection')}
            options={connections.map((c) => ({ value: c.id, label: c.name }))}
            size="sm"
          />
        </div>
        <IconButton label={t('tree.add')} size="sm" onClick={() => openConnectionDialog(null)}>
          <Plus size={14} />
        </IconButton>
      </div>
      <div className="ml-auto flex items-center gap-1">
        <DropdownMenu
          items={(['system', 'light', 'dark'] as Theme[]).map((v) => ({
            id: v,
            label: t(`theme.${v}`),
            onSelect: () => setTheme(v),
          }))}
        >
          <IconButton label={t('header.theme')}>
            <ThemeIcon size={15} />
          </IconButton>
        </DropdownMenu>
        <IconButton label={t('header.settings')} onClick={() => setSettingsOpen(true)}>
          <Settings size={15} />
        </IconButton>
        <IconButton label={ABOUT_LABEL[getLang()]} onClick={() => setAboutOpen(true)}>
          <Info size={15} />
        </IconButton>
      </div>
      {AboutDialog && <AboutDialog open={aboutOpen} onOpenChange={setAboutOpen} />}
    </header>
  );
}
