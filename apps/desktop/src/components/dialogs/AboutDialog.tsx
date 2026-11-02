'use client';

// About BulkDB (DESIGN.md §5.I, parity row 51): version, platform and the app's folders (each
// with "open folder"), the database drivers (tier, availability) and the diagnostics bundle —
// logs and masked config zipped into a folder the user picks, for support requests. Opened
// through `ui.aboutOpen` (palette, Welcome footer, Settings, macOS App menu); no props.

import type { DiagnosticsBundle, DriverInfo, DriverTier } from 'ipc-types';
import { Check, Copy, Database, FolderOpen, PackageOpen } from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { appInfo, diagnosticsBundle, openPath } from '@/lib/commands';
import { pickDirectory, revealPath } from '@/lib/dialogs';
import { formatBytes } from '@/lib/format';
import { errorText } from '@/lib/ipc-error';
import { copyText } from '@/lib/utils';
import { Badge, Button, Dialog, Notice, Spinner, Tooltip } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore } from '@/stores/ui';
import { ab } from './AboutDialog.strings';

/** `app_info` result. */
export type AppInfoData = Awaited<ReturnType<typeof appInfo>>;

const PLATFORM_NAMES: Record<string, string> = { windows: 'Windows', macos: 'macOS', linux: 'Linux', browser: 'Browser (mock)' };

export function platformName(platform: string): string {
  return PLATFORM_NAMES[platform] ?? platform;
}

function tierLabel(tier: DriverTier): string {
  switch (tier) {
    case 'builtin':
      return ab('tier.builtin');
    case 'vendor-client':
      return ab('tier.vendor-client');
    case 'odbc':
      return ab('tier.odbc');
    case 'plugin':
      return ab('tier.plugin');
    default:
      return String(tier);
  }
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

/** Shorten a long path in the middle, keeping the drive / root and the last segments. */
export function middleTruncate(path: string, max = 52): string {
  if (path.length <= max) return path;
  const keep = max - 1;
  const head = Math.ceil(keep * 0.4);
  return `${path.slice(0, head)}…${path.slice(path.length - (keep - head))}`;
}

export function AboutDialog() {
  const open = useUiStore((s) => s.aboutOpen);
  if (!open) return null;
  return <AboutContent onClose={() => useUiStore.getState().setAboutOpen(false)} />;
}

function InfoRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="flex min-h-(--h-control) items-center justify-end text-right text-fg-secondary">{label}</dt>
      <dd className="flex min-h-(--h-control) min-w-0 items-center gap-2">{children}</dd>
    </>
  );
}

function FolderRow({ label, path, onOpen }: { label: string; path: string; onOpen: (path: string) => void }) {
  return (
    <InfoRow label={label}>
      <Tooltip content={path}>
        <span data-selectable className="min-w-0 flex-1 truncate">
          {middleTruncate(path)}
        </span>
      </Tooltip>
      <Button
        size="sm"
        variant="secondary"
        icon={<FolderOpen size={14} strokeWidth={1.75} />}
        onClick={() => onOpen(path)}
        aria-label={`${ab('openFolder')}: ${label}`}
      >
        {ab('openFolder')}
      </Button>
    </InfoRow>
  );
}

function AboutContent({ onClose }: { onClose: () => void }) {
  const drivers = useConnectionsStore((s) => s.drivers);
  const [info, setInfo] = useState<AppInfoData | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [driversError, setDriversError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [bundle, setBundle] = useState<DiagnosticsBundle | null>(null);
  const [bundling, setBundling] = useState(false);
  const copiedTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    let live = true;
    appInfo().then(
      (i) => live && setInfo(i),
      (e: unknown) => live && setInfoError(errorText(e)),
    );
    return () => {
      live = false;
      clearTimeout(copiedTimer.current);
    };
  }, []);

  // The driver list normally arrives with the connections; load it if the store has none yet.
  const needDrivers = drivers.length === 0;
  useEffect(() => {
    if (!needDrivers) return;
    let live = true;
    useConnectionsStore
      .getState()
      .loadDrivers()
      .catch((e: unknown) => live && setDriversError(errorText(e)));
    return () => {
      live = false;
    };
  }, [needDrivers]);

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
    if (!info || !(await copyText(aboutText(info, drivers)))) return;
    setCopied(true);
    clearTimeout(copiedTimer.current);
    copiedTimer.current = setTimeout(() => setCopied(false), 1200);
  };

  const availableCount = drivers.filter((d) => d.available).length;

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={ab('title')}
      width="md"
      footerStart={
        <Button
          variant="ghost"
          icon={copied ? <Check size={14} strokeWidth={2} className="text-success" /> : <Copy size={14} strokeWidth={1.75} />}
          onClick={() => void copyInfo()}
          disabled={!info}
        >
          {copied ? ab('copied') : ab('copyInfo')}
          <span aria-live="polite" className="sr-only">
            {copied ? ab('copied') : ''}
          </span>
        </Button>
      }
      footer={
        <Button variant="secondary" onClick={onClose}>
          {ab('close')}
        </Button>
      }
    >
      <div className="flex flex-col gap-4">
        <div className="flex items-center gap-3">
          <span className="flex h-11 w-11 shrink-0 items-center justify-center rounded-lg bg-accent-soft text-accent">
            <Database size={22} strokeWidth={1.75} aria-hidden />
          </span>
          <div className="min-w-0">
            <div className="text-lg font-semibold">BulkDB</div>
            <div className="text-sm text-fg-secondary">{ab('tagline')}</div>
          </div>
        </div>

        {error && (
          <Notice tone="danger" onClose={() => setError(null)}>
            {error}
          </Notice>
        )}

        {infoError ? (
          <Notice tone="danger">{infoError}</Notice>
        ) : !info ? (
          <div className="flex items-center gap-2 text-sm text-fg-secondary">
            <Spinner size={12} /> {ab('loading')}
          </div>
        ) : (
          <dl className="grid grid-cols-[110px_minmax(0,1fr)] gap-x-3 gap-y-0.5 text-base">
            <InfoRow label={ab('version')}>
              <span data-selectable className="tabular-nums">
                {info.version}
              </span>
            </InfoRow>
            <InfoRow label={ab('platform')}>
              <span>{platformName(info.platform)}</span>
            </InfoRow>
            <InfoRow label={ab('arch')}>
              <span>{info.arch}</span>
            </InfoRow>
            <FolderRow label={ab('configDir')} path={info.configDir} onOpen={(p) => void open(p)} />
            <FolderRow label={ab('logDir')} path={info.logDir} onOpen={(p) => void open(p)} />
          </dl>
        )}

        <section aria-label={ab('drivers')} className="flex flex-col gap-1 border-t border-separator pt-3">
          <div className="flex items-baseline gap-2">
            <h3 className="text-sm font-semibold text-fg-secondary">{ab('drivers')}</h3>
            {drivers.length > 0 && (
              <span className="text-sm text-fg-secondary">{ab('driversSummary', { total: drivers.length, available: availableCount })}</span>
            )}
          </div>
          {driversError ? (
            <Notice tone="warning">
              {ab('noDrivers')}: {driversError}
            </Notice>
          ) : drivers.length === 0 ? (
            <div className="flex items-center gap-2 text-sm text-fg-secondary">
              <Spinner size={12} /> {ab('loading')}
            </div>
          ) : (
            <ul className="list-scroll max-h-48 divide-y divide-separator">
              {drivers.map((d) => (
                <li key={d.id} className="flex h-(--h-row) min-w-0 items-center gap-3 text-base">
                  <span className="min-w-0 flex-1 truncate" title={d.id}>
                    {d.name}
                  </span>
                  <span className="w-28 shrink-0 truncate text-sm text-fg-secondary">{tierLabel(d.tier)}</span>
                  <span className="flex w-20 shrink-0 justify-end">
                    {d.available ? (
                      <Badge tone="success">{ab('available')}</Badge>
                    ) : (
                      <Tooltip content={d.detail ?? d.installHint}>
                        <Badge tone="warning">{ab('missing')}</Badge>
                      </Tooltip>
                    )}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section aria-label={ab('diagnostics')} className="flex flex-col gap-2 border-t border-separator pt-3">
          <div className="flex items-center gap-3">
            <div className="min-w-0 flex-1">
              <h3 className="text-sm font-semibold text-fg-secondary">{ab('diagnostics')}</h3>
              <div className="text-sm text-fg-secondary">{ab('diagnosticsHint')}</div>
            </div>
            <Button variant="secondary" icon={<PackageOpen size={14} strokeWidth={1.75} />} onClick={() => void createBundle()} loading={bundling}>
              {ab('createBundle')}
            </Button>
          </div>
          {bundle && (
            <Notice tone="success" title={ab('bundleDone')}>
              <div className="flex items-center gap-2">
                <span className="min-w-0 flex-1 break-all text-sm" data-testid="bundle-path">
                  {bundle.path}
                </span>
                <Button size="sm" variant="secondary" icon={<FolderOpen size={14} strokeWidth={1.75} />} onClick={() => void reveal(bundle.path)}>
                  {ab('openFolder')}
                </Button>
              </div>
              <div className="text-sm text-fg-secondary">
                {formatBytes(bundle.sizeBytes)}
                {bundle.included.length > 0 && <> · {ab('included', { list: bundle.included.join(', ') })}</>}
              </div>
            </Notice>
          )}
        </section>
      </div>
    </Dialog>
  );
}
