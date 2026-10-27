'use client';

import { Database, Info, Monitor, Moon, Plus, Settings, Sun } from 'lucide-react';
import { useState } from 'react';
import { getLang, t } from '@/lib/i18n';
import { inTauri } from '@/lib/ipc';
import { AboutDialog } from '@/components/dialogs/AboutDialog';
import { Badge, DropdownMenu, IconButton, Select, Tooltip } from '@/components/ui';
import { useConnectionsStore, type ConnStatus } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { useUiStore, type Theme } from '@/stores/ui';

export const STATUS_DOT: Record<ConnStatus, string> = {
  unknown: 'bg-muted-foreground/50',
  testing: 'bg-warning animate-pulse',
  ok: 'bg-success',
  error: 'bg-danger',
};

export function StatusDot({ status, title }: { status: ConnStatus; title?: string }) {
  return (
    <Tooltip content={title ?? t(`tree.status.${status}`)}>
      <span data-status={status} className={`inline-block h-2 w-2 shrink-0 rounded-full ${STATUS_DOT[status]}`} />
    </Tooltip>
  );
}

const themeIcon: Record<Theme, typeof Sun> = { system: Monitor, light: Sun, dark: Moon };

/** Label of the About button (not in `lib/i18n`, kept with the header). */
const ABOUT_LABEL = { ko: 'BulkDB 정보', en: 'About BulkDB' } as const;

export function HeaderBar() {
  const connections = useConnectionsStore((s) => s.connections);
  const selectedId = useConnectionsStore((s) => s.selectedId);
  const status = useConnectionsStore((s) => (s.selectedId ? (s.status[s.selectedId] ?? 'unknown') : 'unknown'));
  const select = useConnectionsStore((s) => s.select);
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);
  const openConnectionDialog = useUiStore((s) => s.openConnectionDialog);
  const setSettingsOpen = useUiStore((s) => s.setSettingsOpen);
  const [aboutOpen, setAboutOpen] = useState(false);
  const ThemeIcon = themeIcon[theme];

  const onSelect = (id: string) => {
    select(id);
    useExtractStore.getState().setConnId(id);
  };

  return (
    <header className="flex h-9 shrink-0 items-center gap-2 border-b border-border bg-muted px-2">
      <div className="flex items-center gap-1.5 pr-2">
        <Database size={16} className="text-accent" />
        <span className="text-[13px] font-semibold">{t('app.name')}</span>
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
      <AboutDialog open={aboutOpen} onOpenChange={setAboutOpen} />
    </header>
  );
}
