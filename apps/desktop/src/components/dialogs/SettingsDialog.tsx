'use client';

import type { Settings } from 'ipc-types';
import { useEffect, useState } from 'react';
import { diagnosticsBundle } from '@/lib/commands';
import { pickDirectory } from '@/lib/dialogs';
import { t, type Lang } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Checkbox, Dialog, Field, Input, Notice, NumberInput, Select } from '@/components/ui';
import { useSettingsStore } from '@/stores/settings';
import { useUiStore, type Theme } from '@/stores/ui';

const LOG_LEVELS = ['trace', 'debug', 'info', 'warn', 'error'];

export function SettingsDialog() {
  const open = useUiStore((s) => s.settingsOpen);
  const setOpen = useUiStore((s) => s.setSettingsOpen);
  const settings = useSettingsStore((s) => s.settings);
  const load = useSettingsStore((s) => s.load);

  useEffect(() => {
    if (open && !settings) void load().catch(() => undefined);
  }, [open, settings, load]);

  if (!open) return null;
  // Keyed on availability so the form initialises once the settings have loaded.
  return <SettingsForm key={settings ? 'ready' : 'loading'} settings={settings} onClose={() => setOpen(false)} />;
}

function SettingsForm({ settings, onClose }: { settings: Settings | null; onClose: () => void }) {
  const theme = useUiStore((s) => s.theme);
  const setTheme = useUiStore((s) => s.setTheme);
  const lang = useUiStore((s) => s.lang);
  const setLang = useUiStore((s) => s.setLang);
  const saving = useSettingsStore((s) => s.saving);
  const save = useSettingsStore((s) => s.save);

  const [form, setForm] = useState<Settings | null>(() => (settings ? { ...settings, ui: { ...settings.ui } } : null));
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const patch = (p: Partial<Settings>) => setForm((f) => (f ? { ...f, ...p } : f));

  const onSave = async () => {
    if (!form) return;
    setError(null);
    try {
      await save(form);
      setNotice(t('settings.saved'));
    } catch (e) {
      setError(errorText(e));
    }
  };

  const onBundle = async () => {
    const dir = await pickDirectory(form?.defaultOutputDir ?? undefined);
    if (!dir) return;
    try {
      const b = await diagnosticsBundle(dir);
      setNotice(t('settings.bundleCreated', { path: b.path }));
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t('settings.title')}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            {t('common.close')}
          </Button>
          <Button variant="primary" onClick={() => void onSave()} loading={saving} disabled={!form}>
            {t('settings.save')}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
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
        <div className="grid grid-cols-2 gap-3">
          <Field label={t('header.theme')} htmlFor="set-theme">
            <Select<Theme>
              id="set-theme"
              value={theme}
              onChange={setTheme}
              options={(['system', 'light', 'dark'] as Theme[]).map((v) => ({ value: v, label: t(`theme.${v}`) }))}
            />
          </Field>
          <Field label={t('settings.language')} htmlFor="set-lang">
            <Select<Lang>
              id="set-lang"
              value={lang}
              onChange={setLang}
              options={[
                { value: 'ko', label: '한국어' },
                { value: 'en', label: 'English' },
              ]}
            />
          </Field>
        </div>
        {form && (
          <>
            <div className="grid grid-cols-2 gap-3">
              <Field label={t('settings.maxJobs')} htmlFor="set-jobs">
                <NumberInput id="set-jobs" min={1} max={3} value={form.maxConcurrentJobs} onChange={(v) => patch({ maxConcurrentJobs: v ?? 1 })} />
              </Field>
              <Field label={t('settings.logLevel')} htmlFor="set-log">
                <Select id="set-log" value={form.logLevel} onChange={(logLevel) => patch({ logLevel })} options={LOG_LEVELS.map((l) => ({ value: l, label: l }))} />
              </Field>
              <Field label={t('settings.defaultOutputDir')} htmlFor="set-outdir" className="col-span-2">
                <div className="flex gap-1">
                  <Input id="set-outdir" value={form.defaultOutputDir ?? ''} onChange={(e) => patch({ defaultOutputDir: e.target.value || undefined })} />
                  <Button variant="outline" onClick={() => void pickDirectory(form.defaultOutputDir ?? undefined).then((p) => p && patch({ defaultOutputDir: p }))}>
                    {t('extract.browse')}
                  </Button>
                </div>
              </Field>
              <Field label={t('settings.tunnelIdle')} htmlFor="set-tunnel">
                <NumberInput id="set-tunnel" min={0} value={form.tunnelIdleSecs} onChange={(v) => patch({ tunnelIdleSecs: v ?? 60 })} />
              </Field>
              <Field label={t('settings.monitorInterval')} htmlFor="set-monitor">
                <NumberInput id="set-monitor" min={500} step={500} value={form.monitorIntervalMs} onChange={(v) => patch({ monitorIntervalMs: v ?? 3000 })} />
              </Field>
              <Field label={t('settings.progressInterval')} htmlFor="set-progress">
                <NumberInput id="set-progress" min={100} step={100} value={form.progressIntervalMs} onChange={(v) => patch({ progressIntervalMs: v ?? 200 })} />
              </Field>
            </div>
            <Field inline label={t('settings.csvCrlf')} htmlFor="set-crlf">
              <Checkbox id="set-crlf" checked={form.csvCrlfDefault} onChange={(csvCrlfDefault) => patch({ csvCrlfDefault })} />
            </Field>
            <Field inline label={t('settings.verify')} htmlFor="set-verify">
              <Checkbox id="set-verify" checked={form.verifyAfterExport} onChange={(verifyAfterExport) => patch({ verifyAfterExport })} />
            </Field>
          </>
        )}
        <div className="flex items-center gap-2 border-t border-border pt-3">
          <div className="flex-1">
            <div className="text-xs font-medium">{t('settings.diagnostics')}</div>
            <div className="text-xs text-muted-foreground">{t('settings.diagnosticsHint')}</div>
          </div>
          <Button variant="outline" onClick={() => void onBundle()}>
            {t('settings.createBundle')}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
