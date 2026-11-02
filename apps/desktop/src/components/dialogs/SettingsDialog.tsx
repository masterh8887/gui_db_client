'use client';

// Settings (DESIGN.md §5.I, parity rows 50 and 52): theme and language (applied at once), jobs,
// output, tunnels and monitor intervals, log level, CRLF and verify defaults, and the
// diagnostics bundle. Footer: [BulkDB 정보] on the left, [닫기] [저장] + saved notice.

import type { DiagnosticsBundle, Settings } from 'ipc-types';
import { Check, FolderOpen } from 'lucide-react';
import { useEffect, useState, type ReactNode } from 'react';
import { diagnosticsBundle } from '@/lib/commands';
import { pickDirectory, revealPath } from '@/lib/dialogs';
import { formatBytes } from '@/lib/format';
import type { Lang } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Checkbox, Dialog, Input, Notice, NumberInput, SegmentedControl, Select, Spinner } from '@/components/ui';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore, type Theme } from '@/stores/ui';
import { ss } from './SettingsDialog.strings';

const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

/** Fields this dialog edits (saving compares only these). */
const EDITABLE: readonly (keyof Settings)[] = [
  'maxConcurrentJobs',
  'defaultOutputDir',
  'csvCrlfDefault',
  'logLevel',
  'tunnelIdleSecs',
  'monitorIntervalMs',
  'progressIntervalMs',
  'verifyAfterExport',
];

export function SettingsDialog() {
  const open = useUiStore((s) => s.settingsOpen);
  const settings = useSettingsStore((s) => s.settings);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    if (!open || settings) return;
    let live = true;
    useSettingsStore
      .getState()
      .load()
      .then(
        () => live && setLoadError(null),
        (e: unknown) => live && setLoadError(errorText(e)),
      );
    return () => {
      live = false;
    };
  }, [open, settings]);

  if (!open) return null;
  return <SettingsForm settings={settings} loadError={loadError} onClose={() => useUiStore.getState().setSettingsOpen(false)} />;
}

function Section({ title, first = false }: { title: string; first?: boolean }) {
  return (
    <h3 className={first ? 'col-span-2 text-sm font-semibold text-fg-secondary' : 'col-span-2 mt-1 border-t border-separator pt-3 text-sm font-semibold text-fg-secondary'}>
      {title}
    </h3>
  );
}

function Row({ label, htmlFor, hint, children }: { label?: string; htmlFor?: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <>
      {label ? (
        <label htmlFor={htmlFor} className="flex min-h-(--h-control) items-center justify-end text-right text-base text-fg-secondary">
          {label}
        </label>
      ) : (
        <span />
      )}
      <div className="flex min-w-0 flex-col justify-center gap-1">
        {children}
        {hint && <span className="text-sm text-fg-secondary">{hint}</span>}
      </div>
    </>
  );
}

function Unit({ children }: { children: ReactNode }) {
  return <span className="text-sm text-fg-secondary">{children}</span>;
}

function SettingsForm({ settings, loadError, onClose }: { settings: Settings | null; loadError: string | null; onClose: () => void }) {
  const theme = useUiStore((s) => s.theme);
  const lang = useUiStore((s) => s.lang);
  const saving = useSettingsStore((s) => s.saving);

  // Only the edits are local; the form is the stored settings plus the edits, so it fills in
  // once the settings load and follows a successful save without remounting.
  const [edits, setEdits] = useState<Partial<Settings>>({});
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [bundle, setBundle] = useState<DiagnosticsBundle | null>(null);
  const [bundling, setBundling] = useState(false);

  const form: Settings | null = settings ? { ...settings, ...edits } : null;
  const dirty = !!form && !!settings && EDITABLE.some((k) => form[k] !== settings[k]);

  const patch = (p: Partial<Settings>) => {
    setSaved(false);
    setEdits((e) => ({ ...e, ...p }));
  };

  const onSave = async () => {
    if (!form) return;
    setError(null);
    try {
      await useSettingsStore.getState().save(form);
      setSaved(true);
    } catch (e) {
      setError(errorText(e));
    }
  };

  const chooseDir = async () => {
    const dir = await pickDirectory(form?.defaultOutputDir ?? undefined);
    if (dir) patch({ defaultOutputDir: dir });
  };

  const onBundle = async () => {
    setError(null);
    const dir = await pickDirectory(form?.defaultOutputDir ?? undefined);
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

  const reveal = async (path: string) => {
    try {
      await revealPath(path);
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={ss('title')}
      width="md"
      footerStart={
        <Button variant="ghost" size="md" onClick={() => useUiStore.getState().setAboutOpen(true)}>
          {ss('about')}
        </Button>
      }
      footer={
        <>
          {saved && !dirty && (
            <span role="status" className="mr-1 flex items-center gap-1 text-sm text-success">
              <Check size={14} strokeWidth={2} />
              {ss('saved')}
            </span>
          )}
          <Button variant="secondary" onClick={onClose}>
            {ss('close')}
          </Button>
          <Button variant="primary" onClick={() => void onSave()} loading={saving} disabled={!dirty}>
            {ss('save')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {(error ?? loadError) && (
          <Notice tone="danger" onClose={error ? () => setError(null) : undefined}>
            {error ?? loadError}
          </Notice>
        )}
        <div className="grid grid-cols-[150px_minmax(0,1fr)] gap-x-3 gap-y-2">
          <Section title={ss('sectionGeneral')} first />
          <Row label={ss('theme')}>
            <SegmentedControl<Theme>
              aria-label={ss('theme')}
              size="md"
              className="self-start"
              value={theme}
              onChange={(v) => useUiStore.getState().setTheme(v)}
              options={[
                { value: 'system', label: ss('themeSystem') },
                { value: 'light', label: ss('themeLight') },
                { value: 'dark', label: ss('themeDark') },
              ]}
            />
          </Row>
          <Row label={ss('language')}>
            <SegmentedControl<Lang>
              aria-label={ss('language')}
              size="md"
              className="self-start"
              value={lang}
              onChange={(v) => useUiStore.getState().setLang(v)}
              options={[
                { value: 'ko', label: '한국어' },
                { value: 'en', label: 'English' },
              ]}
            />
          </Row>

          {!form ? (
            !loadError && (
              <Row>
                <span className="flex items-center gap-2 text-sm text-fg-secondary">
                  <Spinner size={12} /> {ss('loading')}
                </span>
              </Row>
            )
          ) : (
            <>
              <Section title={ss('sectionJobs')} />
              <Row label={ss('maxJobs')} htmlFor="set-jobs" hint={ss('maxJobsHint')}>
                <NumberInput
                  id="set-jobs"
                  className="w-20"
                  integer
                  min={1}
                  max={3}
                  value={form.maxConcurrentJobs}
                  onChange={(v) => patch({ maxConcurrentJobs: v ?? 1 })}
                />
              </Row>
              <Row hint={ss('verifyHint')}>
                <label htmlFor="set-verify" className="flex min-h-(--h-control) items-center gap-2 text-base">
                  <Checkbox id="set-verify" checked={form.verifyAfterExport} onChange={(verifyAfterExport) => patch({ verifyAfterExport })} />
                  {ss('verify')}
                </label>
              </Row>

              <Section title={ss('sectionOutput')} />
              <Row label={ss('outputDir')} htmlFor="set-outdir">
                <div className="flex gap-2">
                  <Input
                    id="set-outdir"
                    data-selectable
                    placeholder={ss('outputDirPlaceholder')}
                    value={form.defaultOutputDir ?? ''}
                    onChange={(e) => patch({ defaultOutputDir: e.target.value || undefined })}
                  />
                  <Button variant="secondary" onClick={() => void chooseDir()}>
                    {ss('browse')}
                  </Button>
                </div>
              </Row>
              <Row>
                <label htmlFor="set-crlf" className="flex min-h-(--h-control) items-center gap-2 text-base">
                  <Checkbox id="set-crlf" checked={form.csvCrlfDefault} onChange={(csvCrlfDefault) => patch({ csvCrlfDefault })} />
                  {ss('crlf')}
                </label>
              </Row>

              <Section title={ss('sectionConnections')} />
              <Row label={ss('tunnelIdle')} htmlFor="set-tunnel">
                <span className="flex items-center gap-2">
                  <NumberInput id="set-tunnel" className="w-24" integer min={0} value={form.tunnelIdleSecs} onChange={(v) => patch({ tunnelIdleSecs: v ?? 60 })} />
                  <Unit>{ss('seconds')}</Unit>
                </span>
              </Row>
              <Row label={ss('monitorInterval')} htmlFor="set-monitor">
                <span className="flex items-center gap-2">
                  <NumberInput
                    id="set-monitor"
                    className="w-24"
                    integer
                    min={500}
                    step={500}
                    value={form.monitorIntervalMs}
                    onChange={(v) => patch({ monitorIntervalMs: v ?? 3000 })}
                  />
                  <Unit>{ss('millis')}</Unit>
                </span>
              </Row>
              <Row label={ss('progressInterval')} htmlFor="set-progress">
                <span className="flex items-center gap-2">
                  <NumberInput
                    id="set-progress"
                    className="w-24"
                    integer
                    min={100}
                    step={100}
                    value={form.progressIntervalMs}
                    onChange={(v) => patch({ progressIntervalMs: v ?? 200 })}
                  />
                  <Unit>{ss('millis')}</Unit>
                </span>
              </Row>

              <Section title={ss('sectionLog')} />
              <Row label={ss('logLevel')} htmlFor="set-log">
                <Select
                  id="set-log"
                  className="w-32"
                  value={form.logLevel}
                  onChange={(logLevel) => patch({ logLevel })}
                  options={LOG_LEVELS.map((l) => ({ value: l, label: l }))}
                />
              </Row>
            </>
          )}

          <Section title={ss('sectionDiagnostics')} />
          <Row label={ss('diagnostics')} hint={ss('diagnosticsHint')}>
            <div>
              <Button variant="secondary" onClick={() => void onBundle()} loading={bundling}>
                {ss('createBundle')}
              </Button>
            </div>
          </Row>
          {bundle && (
            <Row>
              <Notice tone="success" title={ss('bundleCreated')}>
                <div className="flex items-center gap-2">
                  <span data-testid="settings-bundle-path" className="min-w-0 flex-1 break-all text-sm">
                    {bundle.path}
                  </span>
                  <Button size="sm" variant="secondary" icon={<FolderOpen size={14} strokeWidth={1.75} />} onClick={() => void reveal(bundle.path)}>
                    {ss('showInFolder')}
                  </Button>
                </div>
                <div className="text-sm text-fg-secondary">{formatBytes(bundle.sizeBytes)}</div>
              </Notice>
            </Row>
          )}
        </div>
      </div>
    </Dialog>
  );
}
