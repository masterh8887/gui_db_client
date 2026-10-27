'use client';

// New / edit connection. The form is driven by `DriverInfo.fields`; secrets are typed here,
// sent once (in `conn_save` or `conn_test`) and cleared from React state right after.

import type { Connection, ConnectionInput, DriverInfo, ProbeResult, SshAuth } from 'ipc-types';
import { ExternalLink, ScanSearch } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { openUrl, pickAnyFile, pickDirectory } from '@/lib/dialogs';
import { formatDuration } from '@/lib/format';
import { getLang, t, type I18nKey } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { Button, Checkbox, Dialog, Field, Input, Notice, Select, Switch } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';

/** Fields that map to top-level `Connection` members; everything else goes into `fields`. */
const TOP_LEVEL = new Set(['host', 'port', 'database', 'user', 'password']);
const BOOL_FIELDS = new Set(['encrypt', 'trust_server_certificate']);
const SSLMODES = ['disable', 'allow', 'prefer', 'require', 'verify-ca', 'verify-full'];

/** Labels of the "delete the stored secret" checkboxes, per UI language. */
const CLEAR_SECRET_LABELS = {
  ko: { password: '저장된 비밀번호 삭제', ssh: '저장된 SSH 비밀 삭제' },
  en: { password: 'Delete the stored password', ssh: 'Delete the stored SSH secret' },
} as const;

function clearSecretLabel(which: 'password' | 'ssh'): string {
  return CLEAR_SECRET_LABELS[getLang()][which];
}

interface FormState {
  id: string;
  name: string;
  driver: string;
  host: string;
  port: string;
  database: string;
  user: string;
  fields: Record<string, string>;
  password: string;
  savePassword: boolean;
  outputDir: string;
  sshEnabled: boolean;
  sshHost: string;
  sshPort: string;
  sshUser: string;
  sshAuth: SshAuth;
  sshKeyPath: string;
  sshCompression: boolean;
  sshPassword: string;
  sshKeyPassphrase: string;
  /** Delete the stored database password on save (existing connections only). */
  clearPassword: boolean;
  /** Delete the stored SSH password / key passphrase on save (existing connections only). */
  clearSshSecret: boolean;
}

function emptyForm(driver: DriverInfo | undefined): FormState {
  return {
    id: '',
    name: '',
    driver: driver?.id ?? 'postgres',
    host: '',
    port: driver?.defaults.port ?? '',
    database: '',
    user: '',
    fields: Object.fromEntries(Object.entries(driver?.defaults ?? {}).filter(([k]) => !TOP_LEVEL.has(k)).map(([k, v]) => [k, v ?? ''])),
    password: '',
    savePassword: true,
    outputDir: '',
    sshEnabled: false,
    sshHost: '',
    sshPort: '22',
    sshUser: '',
    sshAuth: 'key',
    sshKeyPath: '',
    sshCompression: true,
    sshPassword: '',
    sshKeyPassphrase: '',
    clearPassword: false,
    clearSshSecret: false,
  };
}

function fromConnection(c: Connection): FormState {
  const fields: Record<string, string> = {};
  for (const [k, v] of Object.entries(c.fields)) if (v !== undefined) fields[k] = v;
  return {
    id: c.id,
    name: c.name,
    driver: c.driver,
    host: c.host ?? '',
    port: c.port === undefined ? '' : String(c.port),
    database: c.database ?? '',
    user: c.user ?? '',
    fields,
    password: '',
    savePassword: c.savePassword,
    outputDir: c.outputDir ?? '',
    sshEnabled: c.ssh?.enabled ?? false,
    sshHost: c.ssh?.host ?? '',
    sshPort: String(c.ssh?.port ?? 22),
    sshUser: c.ssh?.user ?? '',
    sshAuth: c.ssh?.auth ?? 'key',
    sshKeyPath: c.ssh?.keyPath ?? '',
    sshCompression: c.ssh?.compression ?? true,
    sshPassword: '',
    sshKeyPassphrase: '',
    clearPassword: false,
    clearSshSecret: false,
  };
}

function toInput(f: FormState, existing: Connection | undefined, driver: DriverInfo | undefined): ConnectionInput {
  const fields: Record<string, string> = {};
  for (const k of driver?.fields ?? []) {
    if (TOP_LEVEL.has(k)) continue;
    const v = f.fields[k];
    if (v !== undefined && v !== '') fields[k] = v;
  }
  const port = f.port.trim() === '' ? undefined : Number(f.port);
  return {
    // Empty for a new connection: the backend assigns the id.
    id: f.id || existing?.id || '',
    name: f.name.trim(),
    driver: f.driver,
    host: f.host.trim() || undefined,
    port: port !== undefined && Number.isFinite(port) ? port : undefined,
    database: f.database.trim() || undefined,
    user: f.user.trim() || undefined,
    fields,
    ssh:
      f.sshEnabled || existing?.ssh
        ? {
            enabled: f.sshEnabled,
            host: f.sshHost.trim(),
            port: Number(f.sshPort) || 22,
            user: f.sshUser.trim(),
            auth: f.sshAuth,
            keyPath: f.sshAuth === 'key' ? f.sshKeyPath.trim() || undefined : undefined,
            compression: f.sshCompression,
          }
        : undefined,
    session: existing?.session ?? {},
    outputDir: f.outputDir.trim() || undefined,
    savePassword: f.savePassword,
    hasPassword: existing?.hasPassword ?? false,
    hasSshSecret: existing?.hasSshSecret ?? false,
    // A secret marked for deletion is never sent along with the flag (the backend refuses both).
    password: f.savePassword && f.password && !f.clearPassword ? f.password : undefined,
    sshPassword: f.sshEnabled && f.sshAuth === 'password' && f.sshPassword && !f.clearSshSecret ? f.sshPassword : undefined,
    sshKeyPassphrase: f.sshEnabled && f.sshAuth === 'key' && f.sshKeyPassphrase && !f.clearSshSecret ? f.sshKeyPassphrase : undefined,
    clearPassword: existing?.hasPassword && f.clearPassword ? true : undefined,
    clearSshSecret: existing?.hasSshSecret && f.clearSshSecret ? true : undefined,
  };
}

function fieldLabel(key: string): string {
  const k = `conn.field.${key}` as I18nKey;
  const s = t(k);
  return s === k ? key : s;
}

export function ConnectionDialog() {
  const { open, editId } = useUiStore((s) => s.connectionDialog);
  const close = useUiStore((s) => s.closeConnectionDialog);
  const drivers = useConnectionsStore((s) => s.drivers);
  const existing = useConnectionsStore((s) => s.connections.find((c) => c.id === editId));
  if (!open) return null;
  // Keyed so every open starts from a fresh form (new vs. the connection being edited).
  return <ConnectionForm key={editId ?? 'new'} existing={existing} drivers={drivers} onClose={close} />;
}

function ConnectionForm({ existing, drivers, onClose }: { existing: Connection | undefined; drivers: DriverInfo[]; onClose: () => void }) {
  const save = useConnectionsStore((s) => s.save);
  const test = useConnectionsStore((s) => s.test);
  const rescanDrivers = useConnectionsStore((s) => s.rescanDrivers);
  const refreshConnection = useConnectionsStore((s) => s.refreshConnection);
  const existingId = existing?.id;

  // The list's secret flags were read when it loaded; re-read them so the "delete the stored
  // secret" options match what the keyring holds now.
  useEffect(() => {
    if (!existingId) return;
    refreshConnection(existingId).catch((e: unknown) => logLocal('warn', `conn_get ${existingId}: ${errorText(e)}`));
  }, [existingId, refreshConnection]);
  const [form, setForm] = useState<FormState>(() => (existing ? fromConnection(existing) : emptyForm(drivers.find((d) => d.available))));
  const [busy, setBusy] = useState<'save' | 'test' | 'rescan' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [probe, setProbe] = useState<ProbeResult | null>(null);
  /** Why the driver's install link could not be opened (already in the log panel). */
  const [linkError, setLinkError] = useState<string | null>(null);
  const close = onClose;

  const driver = useMemo(() => drivers.find((d) => d.id === form.driver), [drivers, form.driver]);
  const patch = (p: Partial<FormState>) => setForm((f) => ({ ...f, ...p }));
  const patchField = (k: string, v: string) => setForm((f) => ({ ...f, fields: { ...f.fields, [k]: v } }));
  // After a save the typed secrets are gone and the clear requests have been carried out.
  const clearSecrets = () => patch({ password: '', sshPassword: '', sshKeyPassphrase: '', clearPassword: false, clearSshSecret: false });

  const openInstallLink = async (url: string) => {
    setLinkError(null);
    setLinkError(await openUrl(url));
  };

  const onDriverChange = (id: string) => {
    const d = drivers.find((x) => x.id === id);
    setForm((f) => ({
      ...f,
      driver: id,
      port: f.port || d?.defaults.port || '',
      fields: { ...Object.fromEntries(Object.entries(d?.defaults ?? {}).filter(([k]) => !TOP_LEVEL.has(k)).map(([k, v]) => [k, v ?? ''])), ...f.fields },
    }));
  };

  const validate = (): string | null => {
    if (!form.name.trim()) return t('conn.nameRequired');
    if (driver && !driver.available) return t('conn.driverUnavailable');
    return null;
  };

  const doSave = async (): Promise<Connection | null> => {
    const v = validate();
    if (v) {
      setError(v);
      return null;
    }
    const input = toInput(form, existing, driver);
    const saved = await save(input);
    clearSecrets();
    patch({ id: saved.id });
    return saved;
  };

  const onSave = async () => {
    setBusy('save');
    setError(null);
    try {
      const saved = await doSave();
      if (saved) close();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const onTest = async () => {
    setBusy('test');
    setError(null);
    setProbe(null);
    const oneTimePassword = !form.savePassword && form.password ? form.password : undefined;
    try {
      const saved = await doSave();
      if (!saved) return;
      setProbe(await test(saved.id, oneTimePassword));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  // After installing a vendor client (Oracle Instant Client, an ODBC driver) the registry has to
  // probe again; the refreshed list flows back through the store and clears this notice.
  const onRescan = async () => {
    setBusy('rescan');
    setError(null);
    try {
      await rescanDrivers();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const showPasswordHint = existing?.hasPassword && !form.password && !form.clearPassword;
  const needsPassword = driver?.fields.includes('password') ?? false;
  const keepSshHint = existing?.hasSshSecret && !form.clearSshSecret ? t('conn.passwordKeep') : undefined;

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={existing ? t('conn.title.edit') : t('conn.title.new')}
      width="lg"
      modal={busy !== null}
      footer={
        <>
          <Button variant="ghost" onClick={close} disabled={busy !== null}>
            {t('common.cancel')}
          </Button>
          <Button variant="outline" onClick={() => void onTest()} loading={busy === 'test'} disabled={busy !== null} title={t('conn.saveFirst')}>
            {t('conn.test')}
          </Button>
          <Button variant="primary" onClick={() => void onSave()} loading={busy === 'save'} disabled={busy !== null}>
            {t('conn.save')}
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
        {probe && (
          <Notice tone="success">
            {t('conn.testOk', { driver: probe.server.driver, version: probe.server.version, ms: formatDuration(probe.elapsedMs) })}
            {probe.tunnelUsed && <span className="ml-1 text-xs text-muted-foreground">({t('conn.testTunnel')})</span>}
            {probe.recommended.workMem && <div className="text-xs text-muted-foreground">work_mem {t('extract.recommendation').toLowerCase()}: {probe.recommended.workMem}</div>}
          </Notice>
        )}
        <div className="grid grid-cols-[1fr_200px] gap-3">
          <Field label={t('conn.name')} htmlFor="conn-name">
            <Input id="conn-name" value={form.name} onChange={(e) => patch({ name: e.target.value })} autoFocus />
          </Field>
          <Field label={t('conn.driver')} htmlFor="conn-driver">
            <Select id="conn-driver" value={form.driver} onChange={onDriverChange} options={drivers.map((d) => ({ value: d.id, label: d.available ? d.name : `${d.name} (!)` }))} />
          </Field>
        </div>
        {driver && !driver.available && (
          <Notice tone="warning" title={t('conn.driverUnavailable')}>
            {driver.detail && <div className="text-xs">{driver.detail}</div>}
            {driver.installHint && <div className="text-xs">{driver.installHint}</div>}
            {driver.installUrl && (
              <button type="button" className="mt-1 inline-flex items-center gap-1 text-xs text-accent hover:underline" onClick={() => void openInstallLink(driver.installUrl!)}>
                <ExternalLink size={11} /> {t('conn.install')}: {driver.installLabel ?? driver.installUrl}
              </button>
            )}
            {linkError && (
              <Notice tone="danger" className="mt-1" onClose={() => setLinkError(null)}>
                {linkError}
              </Notice>
            )}
            <div className="mt-1">
              <Button size="sm" variant="outline" icon={<ScanSearch size={12} />} onClick={() => void onRescan()} loading={busy === 'rescan'} disabled={busy !== null}>
                {t('drivers.rescan')}
              </Button>
            </div>
          </Notice>
        )}

        <div className="grid grid-cols-2 gap-3">
          {driver?.fields.includes('host') && (
            <Field label={t('conn.host')} htmlFor="conn-host">
              <Input id="conn-host" value={form.host} onChange={(e) => patch({ host: e.target.value })} />
            </Field>
          )}
          {driver?.fields.includes('port') && (
            <Field label={t('conn.port')} htmlFor="conn-port">
              <Input id="conn-port" inputMode="numeric" value={form.port} onChange={(e) => patch({ port: e.target.value.replace(/[^\d]/g, '') })} />
            </Field>
          )}
          {driver?.fields.includes('database') && (
            <Field label={t('conn.database')} htmlFor="conn-db">
              <Input id="conn-db" value={form.database} onChange={(e) => patch({ database: e.target.value })} />
            </Field>
          )}
          {driver?.fields.includes('user') && (
            <Field label={t('conn.user')} htmlFor="conn-user">
              <Input id="conn-user" value={form.user} onChange={(e) => patch({ user: e.target.value })} />
            </Field>
          )}
          {needsPassword && (
            <Field label={t('conn.password')} htmlFor="conn-password" hint={t('conn.passwordHint')}>
              <Input
                id="conn-password"
                type="password"
                autoComplete="new-password"
                value={form.password}
                disabled={form.clearPassword}
                placeholder={showPasswordHint ? t('conn.passwordKeep') : undefined}
                onChange={(e) => patch({ password: e.target.value })}
              />
            </Field>
          )}
          {needsPassword && (
            <div className="mt-5 flex flex-col">
              <Field inline label={t('conn.savePassword')} htmlFor="conn-savepw">
                <Checkbox id="conn-savepw" checked={form.savePassword} onChange={(savePassword) => patch({ savePassword })} />
              </Field>
              {existing?.hasPassword && (
                <Field inline label={clearSecretLabel('password')} htmlFor="conn-clearpw">
                  <Checkbox id="conn-clearpw" checked={form.clearPassword} onChange={(clearPassword) => patch(clearPassword ? { clearPassword, password: '' } : { clearPassword })} />
                </Field>
              )}
            </div>
          )}
          {driver?.fields
            .filter((k) => !TOP_LEVEL.has(k))
            .map((k) => {
              const id = `conn-f-${k}`;
              if (k === 'sslmode') {
                return (
                  <Field key={k} label={fieldLabel(k)} htmlFor={id}>
                    <Select id={id} value={form.fields[k] ?? 'prefer'} onChange={(v) => patchField(k, v)} options={SSLMODES.map((m) => ({ value: m, label: m }))} />
                  </Field>
                );
              }
              if (BOOL_FIELDS.has(k)) {
                return (
                  <Field key={k} inline label={fieldLabel(k)} htmlFor={id} className="mt-5">
                    <Switch id={id} checked={(form.fields[k] ?? 'false') === 'true'} onChange={(v) => patchField(k, v ? 'true' : 'false')} />
                  </Field>
                );
              }
              if (k === 'driver' && driver.odbcDrivers.length > 0) {
                return (
                  <Field key={k} label={t('conn.odbcDriver')} htmlFor={id}>
                    <Select id={id} value={form.fields[k] || undefined} onChange={(v) => patchField(k, v)} placeholder={t('conn.odbcDriver')} options={driver.odbcDrivers.map((d) => ({ value: d, label: d }))} />
                  </Field>
                );
              }
              if (k === 'path') {
                return (
                  <Field key={k} label={fieldLabel(k)} htmlFor={id} className="col-span-2">
                    <div className="flex gap-1">
                      <Input id={id} value={form.fields[k] ?? ''} onChange={(e) => patchField(k, e.target.value)} placeholder="C:\data\local.db" />
                      <Button variant="outline" onClick={() => void pickAnyFile([{ name: 'SQLite', extensions: ['db', 'sqlite', 'sqlite3'] }]).then((p) => p && patchField(k, p))}>
                        {t('extract.browse')}
                      </Button>
                    </div>
                  </Field>
                );
              }
              return (
                <Field key={k} label={fieldLabel(k)} htmlFor={id} className={k === 'connection_string' ? 'col-span-2' : undefined}>
                  <Input id={id} value={form.fields[k] ?? ''} onChange={(e) => patchField(k, e.target.value)} />
                </Field>
              );
            })}
          <Field label={t('conn.outputDir')} htmlFor="conn-outdir" className="col-span-2">
            <div className="flex gap-1">
              <Input id="conn-outdir" value={form.outputDir} onChange={(e) => patch({ outputDir: e.target.value })} />
              <Button variant="outline" onClick={() => void pickDirectory(form.outputDir).then((p) => p && patch({ outputDir: p }))}>
                {t('extract.browse')}
              </Button>
            </div>
          </Field>
        </div>

        {driver?.fields.includes('host') && (
          <fieldset className="flex flex-col gap-3 rounded border border-border p-3">
            <legend className="px-1 text-xs font-semibold text-muted-foreground">{t('conn.ssh')}</legend>
            <Field inline label={t('conn.sshEnabled')} htmlFor="ssh-enabled">
              <Switch id="ssh-enabled" checked={form.sshEnabled} onChange={(sshEnabled) => patch({ sshEnabled })} />
            </Field>
            {form.sshEnabled && (
              <div className="grid grid-cols-2 gap-3">
                <Field label={t('conn.sshHost')} htmlFor="ssh-host">
                  <Input id="ssh-host" value={form.sshHost} onChange={(e) => patch({ sshHost: e.target.value })} />
                </Field>
                <Field label={t('conn.sshPort')} htmlFor="ssh-port">
                  <Input id="ssh-port" inputMode="numeric" value={form.sshPort} onChange={(e) => patch({ sshPort: e.target.value.replace(/[^\d]/g, '') })} />
                </Field>
                <Field label={t('conn.sshUser')} htmlFor="ssh-user">
                  <Input id="ssh-user" value={form.sshUser} onChange={(e) => patch({ sshUser: e.target.value })} />
                </Field>
                <Field label={t('conn.sshAuth')} htmlFor="ssh-auth">
                  <Select<SshAuth>
                    id="ssh-auth"
                    value={form.sshAuth}
                    onChange={(sshAuth) => patch({ sshAuth })}
                    options={[
                      { value: 'key', label: t('auth.key') },
                      { value: 'password', label: t('auth.password') },
                      { value: 'agent', label: t('auth.agent') },
                    ]}
                  />
                </Field>
                {form.sshAuth === 'key' && (
                  <>
                    <Field label={t('conn.sshKeyPath')} htmlFor="ssh-key">
                      <div className="flex gap-1">
                        <Input id="ssh-key" value={form.sshKeyPath} placeholder="~/.ssh/id_ed25519" onChange={(e) => patch({ sshKeyPath: e.target.value })} />
                        <Button variant="outline" onClick={() => void pickAnyFile().then((p) => p && patch({ sshKeyPath: p }))}>
                          {t('extract.browse')}
                        </Button>
                      </div>
                    </Field>
                    <Field label={t('conn.sshPassphrase')} htmlFor="ssh-pass" hint={keepSshHint}>
                      <Input id="ssh-pass" type="password" autoComplete="new-password" value={form.sshKeyPassphrase} disabled={form.clearSshSecret} onChange={(e) => patch({ sshKeyPassphrase: e.target.value })} />
                    </Field>
                  </>
                )}
                {form.sshAuth === 'password' && (
                  <Field label={t('conn.sshPassword')} htmlFor="ssh-pw" hint={keepSshHint}>
                    <Input id="ssh-pw" type="password" autoComplete="new-password" value={form.sshPassword} disabled={form.clearSshSecret} onChange={(e) => patch({ sshPassword: e.target.value })} />
                  </Field>
                )}
                <Field inline label={t('conn.sshCompression')} htmlFor="ssh-comp">
                  <Checkbox id="ssh-comp" checked={form.sshCompression} onChange={(sshCompression) => patch({ sshCompression })} />
                </Field>
              </div>
            )}
            {/* Offered whenever a secret is stored, even with the tunnel switched off. */}
            {existing?.hasSshSecret && (
              <Field inline label={clearSecretLabel('ssh')} htmlFor="ssh-clear">
                <Checkbox
                  id="ssh-clear"
                  checked={form.clearSshSecret}
                  onChange={(clearSshSecret) => patch(clearSshSecret ? { clearSshSecret, sshPassword: '', sshKeyPassphrase: '' } : { clearSshSecret })}
                />
              </Field>
            )}
          </fieldset>
        )}
      </div>
    </Dialog>
  );
}
