'use client';

// About BulkDB: version, platform and the app's folders (each with "open folder"), a summary of
// the database drivers (tier, availability), and the diagnostics bundle — logs and masked
// config zipped into a folder the user picks, for support requests.

import type { DiagnosticsBundle, DriverInfo, DriverTier } from 'ipc-types';
import { Copy, Database, FolderOpen, PackageOpen } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { appInfo, diagnosticsBundle, openPath } from '@/lib/commands';
import { pickDirectory, revealPath } from '@/lib/dialogs';
import { formatBytes } from '@/lib/format';
import { getLang, t } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { copyText } from '@/lib/utils';
import { Badge, Button, Dialog, Notice, Spinner, Table, Tooltip, type Column } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useSettingsStore } from '@/stores/settings';

/** `app_info` result. */
export type AppInfoData = Awaited<ReturnType<typeof appInfo>>;

// Strings that `lib/i18n` does not have (kept local so this dialog owns its text).
const TEXT = {
  ko: {
    title: 'BulkDB 정보',
    version: '버전',
    platform: '플랫폼',
    arch: '아키텍처',
    configDir: '설정 폴더',
    logDir: '로그 폴더',
    openFolder: '폴더 열기',
    drivers: '드라이버',
    driver: '드라이버',
    tier: '계층',
    status: '상태',
    available: '사용 가능',
    missing: '설치 필요',
    driversSummary: '{total}개 중 {available}개 사용 가능',
    noDrivers: '드라이버 정보를 불러오지 못했습니다',
    diagnostics: '진단 번들',
    createBundle: '진단 번들 만들기',
    bundleDone: '진단 번들을 만들었습니다',
    included: '포함: {list}',
    copyInfo: '정보 복사',
    copied: '복사했습니다',
    'tier.builtin': '내장',
    'tier.vendor-client': '벤더 클라이언트',
    'tier.odbc': 'ODBC',
    'tier.plugin': '플러그인',
  },
  en: {
    title: 'About BulkDB',
    version: 'Version',
    platform: 'Platform',
    arch: 'Architecture',
    configDir: 'Config folder',
    logDir: 'Log folder',
    openFolder: 'Open folder',
    drivers: 'Drivers',
    driver: 'Driver',
    tier: 'Tier',
    status: 'Status',
    available: 'Available',
    missing: 'Not installed',
    driversSummary: '{available} of {total} available',
    noDrivers: 'Could not load the driver list',
    diagnostics: 'Diagnostics bundle',
    createBundle: 'Create diagnostics bundle',
    bundleDone: 'Diagnostics bundle created',
    included: 'Includes: {list}',
    copyInfo: 'Copy info',
    copied: 'Copied',
    'tier.builtin': 'Built-in',
    'tier.vendor-client': 'Vendor client',
    'tier.odbc': 'ODBC',
    'tier.plugin': 'Plugin',
  },
} as const;

type TextKey = keyof (typeof TEXT)['en'];

function tx(key: TextKey, params?: Record<string, string | number>): string {
  let s: string = TEXT[getLang()][key] ?? TEXT.en[key];
  if (params) for (const [k, v] of Object.entries(params)) s = s.split(`{${k}}`).join(String(v));
  return s;
}

const PLATFORM_NAMES: Record<string, string> = { windows: 'Windows', macos: 'macOS', linux: 'Linux', browser: 'Browser (mock)' };

export function platformName(platform: string): string {
  return PLATFORM_NAMES[platform] ?? platform;
}

function tierLabel(tier: DriverTier): string {
  return tx(`tier.${tier}` as TextKey);
}

/** Plain-text summary for support tickets (no secrets: paths, versions and driver states only). */
export function aboutText(info: AppInfoData, drivers: readonly DriverInfo[]): string {
  const lines = [
    `BulkDB ${info.version}`,
    `${platformName(info.platform)} ${info.arch}`,
    `config: ${info.configDir}`,
    `logs: ${info.logDir}`,
    ...drivers.map((d) => `driver ${d.id} (${d.tier}): ${d.available ? 'available' : `missing${d.detail ? ` - ${d.detail}` : ''}`}`),
  ];
  return lines.join('\n');
}

export interface AboutDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AboutDialog({ open, onOpenChange }: AboutDialogProps) {
  if (!open) return null;
  return <AboutContent onClose={() => onOpenChange(false)} />;
}

function InfoRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="flex min-w-0 items-center gap-2">{children}</dd>
    </>
  );
}

function FolderRow({ label, path, onOpen }: { label: string; path: string; onOpen: (path: string) => void }) {
  return (
    <InfoRow label={label}>
      <span className="min-w-0 flex-1 truncate font-mono text-xs" title={path}>
        {path}
      </span>
      <Button size="sm" variant="outline" icon={<FolderOpen size={12} />} onClick={() => onOpen(path)} aria-label={`${tx('openFolder')}: ${label}`}>
        {tx('openFolder')}
      </Button>
    </InfoRow>
  );
}

function AboutContent({ onClose }: { onClose: () => void }) {
  const drivers = useConnectionsStore((s) => s.drivers);
  const loadDrivers = useConnectionsStore((s) => s.loadDrivers);
  const [info, setInfo] = useState<AppInfoData | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [driversError, setDriversError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [bundle, setBundle] = useState<DiagnosticsBundle | null>(null);
  const [bundling, setBundling] = useState(false);

  useEffect(() => {
    let live = true;
    appInfo().then(
      (i) => live && setInfo(i),
      (e: unknown) => live && setInfoError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, []);

  // The driver list normally arrives with the connections; load it if the store has none yet.
  const needDrivers = drivers.length === 0;
  useEffect(() => {
    if (!needDrivers) return;
    let live = true;
    loadDrivers().catch((e: unknown) => live && setDriversError(errorText(e)));
    return () => {
      live = false;
    };
  }, [needDrivers, loadDrivers]);

  const open = async (path: string) => {
    setError(null);
    try {
      await openPath(path);
    } catch (e) {
      setError(errorText(e));
    }
  };

  const reveal = async (path: string) => {
    setError(null);
    try {
      await revealPath(path);
    } catch (e) {
      setError(errorText(e));
    }
  };

  const createBundle = async () => {
    setError(null);
    const dir = await pickDirectory(useSettingsStore.getState().settings?.defaultOutputDir ?? undefined);
    if (!dir) return;
    setBundling(true);
    try {
      setBundle(await diagnosticsBundle(dir));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBundling(false);
    }
  };

  const copyInfo = async () => {
    if (!info) return;
    if (await copyText(aboutText(info, drivers))) setNotice(tx('copied'));
  };

  const availableCount = drivers.filter((d) => d.available).length;
  const columns: Column<DriverInfo>[] = [
    { id: 'name', header: tx('driver'), cell: (d) => <span title={d.id}>{d.name}</span> },
    { id: 'tier', header: tx('tier'), cell: (d) => <span className="text-muted-foreground">{tierLabel(d.tier)}</span>, width: 120 },
    {
      id: 'status',
      header: tx('status'),
      width: 110,
      cell: (d) =>
        d.available ? (
          <Badge tone="success">{tx('available')}</Badge>
        ) : (
          <Tooltip content={d.detail ?? d.installHint}>
            <Badge tone="warning">{tx('missing')}</Badge>
          </Tooltip>
        ),
    },
  ];

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={tx('title')}
      width="lg"
      footer={
        <>
          <Button variant="ghost" icon={<Copy size={13} />} onClick={() => void copyInfo()} disabled={!info} className="mr-auto">
            {tx('copyInfo')}
          </Button>
          <Button variant="ghost" onClick={onClose}>
            {t('common.close')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-2">
          <Database size={22} className="text-accent" aria-hidden />
          <div>
            <div className="text-[15px] font-semibold">{t('app.name')}</div>
            <div className="text-xs text-muted-foreground">{t('app.tagline')}</div>
          </div>
        </div>

        {error && (
          <Notice tone="danger" onClose={() => setError(null)}>
            {error}
          </Notice>
        )}
        {notice && (
          <Notice tone="success" onClose={() => setNotice(null)}>
            {notice}
          </Notice>
        )}

        {infoError ? (
          <Notice tone="danger">{infoError}</Notice>
        ) : !info ? (
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <Spinner size={12} /> {t('common.loading')}
          </div>
        ) : (
          <dl className="grid grid-cols-[110px_minmax(0,1fr)] items-center gap-x-3 gap-y-1.5 text-[13px]">
            <InfoRow label={tx('version')}>
              <span className="font-mono">{info.version}</span>
            </InfoRow>
            <InfoRow label={tx('platform')}>
              <span>{platformName(info.platform)}</span>
            </InfoRow>
            <InfoRow label={tx('arch')}>
              <span className="font-mono">{info.arch}</span>
            </InfoRow>
            <FolderRow label={tx('configDir')} path={info.configDir} onOpen={(p) => void open(p)} />
            <FolderRow label={tx('logDir')} path={info.logDir} onOpen={(p) => void open(p)} />
          </dl>
        )}

        <section className="flex flex-col gap-1.5 border-t border-border pt-3" aria-label={tx('drivers')}>
          <div className="flex items-center gap-2">
            <span className="text-xs font-medium">{tx('drivers')}</span>
            {drivers.length > 0 && (
              <span className="text-xs text-muted-foreground">{tx('driversSummary', { total: drivers.length, available: availableCount })}</span>
            )}
          </div>
          {driversError ? (
            <Notice tone="warning">
              {tx('noDrivers')}: {driversError}
            </Notice>
          ) : (
            <div className="max-h-52 overflow-auto rounded border border-border">
              <Table columns={columns} rows={drivers} rowKey={(d) => d.id} empty={t('common.loading')} />
            </div>
          )}
        </section>

        <section className="flex flex-col gap-1.5 border-t border-border pt-3" aria-label={tx('diagnostics')}>
          <div className="flex items-center gap-2">
            <div className="min-w-0 flex-1">
              <div className="text-xs font-medium">{tx('diagnostics')}</div>
              <div className="text-xs text-muted-foreground">{t('settings.diagnosticsHint')}</div>
            </div>
            <Button variant="outline" icon={<PackageOpen size={13} />} onClick={() => void createBundle()} loading={bundling}>
              {tx('createBundle')}
            </Button>
          </div>
          {bundle && (
            <Notice tone="success" title={tx('bundleDone')}>
              <div className="flex items-center gap-2">
                <span className="min-w-0 flex-1 break-all font-mono text-xs" data-testid="bundle-path">
                  {bundle.path}
                </span>
                <Button size="sm" variant="outline" icon={<FolderOpen size={12} />} onClick={() => void reveal(bundle.path)}>
                  {tx('openFolder')}
                </Button>
              </div>
              <div className="text-xs text-muted-foreground">
                {formatBytes(bundle.sizeBytes)}
                {bundle.included.length > 0 && <> · {tx('included', { list: bundle.included.join(', ') })}</>}
              </div>
            </Notice>
          )}
        </section>
      </div>
    </Dialog>
  );
}
