'use client';

// New / edit connection (DESIGN.md §5.B): a 560px sheet. A new connection starts with the driver
// picker (step 1), then the compact form (step 2); editing opens the form directly. The form is
// driven by `DriverInfo.fields`; secrets are typed here, sent once (in `conn_save` or
// `conn_test`) and cleared from React state right after.
//
// Footer: [테스트] + inline result on the left; [취소] [저장] [연결] on the right. Test = save,
// then test. Connect = save (+ a one-time password test when it is not kept in the keychain),
// close, open the workspace. Editing a connection whose workspace is open: [저장] is primary
// and [연결] is hidden. Every save of an existing connection calls `afterConnectionSaved`.
//
// Registry fields with `DriverInfo.choices` render as a select with localized labels; the MySQL
// compatibility fields (protocol, server encoding, old passwords) sit in a collapsible 호환성
// section that opens by itself when a value differs from the default or the core rejects one
// (`INVALID` naming the field is shown next to it). A probe note asking for an explicit server
// encoding (`probeNotes.ts`) shows as a notice under 호환성 whose button opens it on 서버 인코딩.
// docs/mysql-compat.md §3, §9.

import type { Connection, DriverInfo, ProbeResult, SshAuth } from 'ipc-types';
import { Check, ChevronRight, CircleX, ExternalLink, PenLine, ScanSearch, TriangleAlert } from 'lucide-react';
import { useEffect, useMemo, useState, type KeyboardEvent, type ReactNode } from 'react';
import { flushSync } from 'react-dom';
import { afterConnectionSaved, clearWriteAllowed, rescanDrivers } from '@/lib/connection-actions';
import { openUrl, pickAnyFile, pickDirectory } from '@/lib/dialogs';
import { formatDuration } from '@/lib/format';
import { errorText } from '@/lib/ipc-error';
import { cn } from '@/lib/utils';
import {
  Button,
  Checkbox,
  Input,
  Notice,
  SegmentedControl,
  Select,
  Sheet,
  Switch,
  TagPicker,
  Tooltip,
} from '@/components/ui';
import { DriverBadge, tierLabel } from '@/components/welcome/DriverBadge';
import { useConnectionsStore } from '@/stores/connections';
import { logLocal } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { useWorkspaceStore } from '@/stores/workspace';
import { choiceLabel, cs, fieldHint, fieldLabel } from './ConnectionDialog.strings';
import { DriverPicker } from './ConnectionDialogDriverPicker';
import {
  BOOL_FIELDS,
  COMPAT_FIELDS,
  compatChanged,
  emptyForm,
  fieldDefault,
  fromConnection,
  invalidFieldError,
  SSLMODES,
  TOP_LEVEL,
  toInput,
  withDriver,
  withoutSecrets,
  type FormState,
} from './ConnectionDialogForm';
import { encodingNotes, type EncodingNote } from './probeNotes';

type Busy = 'save' | 'test' | 'connect' | 'rescan' | null;
type TestResult = { ok: true; probe: ProbeResult } | { ok: false; error: string } | null;

export function ConnectionDialog() {
  const { open, editId } = useUiStore((s) => s.connectionDialog);
  const close = useUiStore((s) => s.closeConnectionDialog);
  const drivers = useConnectionsStore((s) => s.drivers);
  const existing = useConnectionsStore((s) => (editId ? s.connections.find((c) => c.id === editId) : undefined));
  if (!open) return null;
  // Keyed so every open starts from a fresh form (new vs. the connection being edited).
  return <ConnectionSheet key={editId ?? 'new'} initial={existing} drivers={drivers} onClose={close} />;
}

function ConnectionSheet({ initial, drivers, onClose }: { initial: Connection | undefined; drivers: DriverInfo[]; onClose: () => void }) {
  const save = useConnectionsStore((s) => s.save);
  const test = useConnectionsStore((s) => s.test);
  const refreshConnection = useConnectionsStore((s) => s.refreshConnection);
  const editing = initial !== undefined;
  const editId = initial?.id;

  const [form, setForm] = useState<FormState>(() =>
    initial ? fromConnection(initial, drivers.find((d) => d.id === initial.driver)) : emptyForm(drivers.find((d) => d.available)),
  );
  // After the first save of a new connection (Test), later saves update that connection.
  const existing = useConnectionsStore((s) => s.connections.find((c) => c.id === (form.id || editId)));
  const workspaceOpen = useWorkspaceStore((s) => !!form.id && s.opened.includes(form.id));
  const [step, setStep] = useState<'driver' | 'form'>(editing ? 'form' : 'driver');
  const [pickerValue, setPickerValue] = useState<string | null>(form.driver);
  const [driverSelect, setDriverSelect] = useState(false);
  const [sshOpen, setSshOpen] = useState(() => form.sshEnabled || !!initial?.hasSshSecret);
  const [compatOpen, setCompatOpen] = useState(() => compatChanged(form, drivers.find((d) => d.id === form.driver)));
  /** `INVALID` messages from the core, keyed by the driver field they name. */
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<Busy>(null);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<TestResult>(null);
  /** Why the driver's install link could not be opened (already in the log). */
  const [linkError, setLinkError] = useState<string | null>(null);

  // The list's secret flags were read when it loaded; re-read them so the "delete the stored
  // secret" options match what the keyring holds now.
  useEffect(() => {
    if (!editId) return;
    refreshConnection(editId).catch((e: unknown) => logLocal('warn', `conn_get ${editId}: ${errorText(e)}`));
  }, [editId, refreshConnection]);

  const driver = useMemo(() => drivers.find((d) => d.id === form.driver), [drivers, form.driver]);
  const patch = (p: Partial<FormState>) => setForm((f) => ({ ...f, ...p }));
  const patchField = (k: string, v: string) => {
    setForm((f) => ({ ...f, fields: { ...f.fields, [k]: v } }));
    setFieldErrors((e) => (k in e ? Object.fromEntries(Object.entries(e).filter(([f]) => f !== k)) : e));
  };
  /** Show an `INVALID` error that names a driver field next to that field; `true` when it did. */
  const showFieldError = (e: unknown): boolean => {
    const fe = invalidFieldError(e, driver?.fields ?? []);
    if (!fe) return false;
    setFieldErrors({ [fe.field]: fe.message });
    if (COMPAT_FIELDS.has(fe.field)) setCompatOpen(true);
    return true;
  };
  const changeDriver = (id: string) => {
    const next = drivers.find((d) => d.id === id);
    setForm((f) => withDriver(f, next, drivers.find((d) => d.id === f.driver)));
  };

  const validate = (): string | null => {
    if (!form.name.trim()) return cs('nameRequired');
    if (driver && !driver.available) return cs('driverUnavailable');
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
    setForm((f) => ({ ...withoutSecrets(f), id: saved.id }));
    if (input.id) void afterConnectionSaved(saved.id);
    return saved;
  };

  /** Run one footer action: busy flag, error notice, never two at once. */
  const run = async (kind: Exclude<Busy, null>, fn: () => Promise<void>) => {
    if (busy) return;
    setBusy(kind);
    setError(null);
    setFieldErrors({});
    try {
      await fn();
    } catch (e) {
      if (!showFieldError(e)) setError(errorText(e));
    } finally {
      setBusy(null);
    }
  };

  const oneTimePassword = () => (!form.savePassword && form.password ? form.password : undefined);

  const onSave = () =>
    run('save', async () => {
      if (await doSave()) onClose();
    });

  const onTest = () =>
    run('test', async () => {
      setResult(null);
      const password = oneTimePassword();
      const saved = await doSave();
      if (!saved) return;
      try {
        setResult({ ok: true, probe: await test(saved.id, password) });
      } catch (e) {
        showFieldError(e);
        setResult({ ok: false, error: errorText(e) });
      }
    });

  const onConnect = () =>
    run('connect', async () => {
      const password = oneTimePassword();
      const saved = await doSave();
      if (!saved) return;
      if (password) {
        // Not kept in the keychain: give it to the backend for this session first.
        try {
          await test(saved.id, password);
        } catch (e) {
          showFieldError(e);
          setResult({ ok: false, error: errorText(e) });
          return;
        }
      }
      onClose();
      void useWorkspaceStore.getState().openWorkspace(saved.id);
    });

  // After installing a vendor client (Oracle Instant Client, an ODBC driver) the registry has to
  // probe again; the refreshed list flows back through the store and clears the notice.
  const onRescan = () => run('rescan', async () => void (await rescanDrivers()));

  const onPick = (id: string) => {
    changeDriver(id);
    setStep('form');
  };

  const primary = workspaceOpen ? onSave : onConnect;
  // Enter in a text field runs the primary action (TablePlus: Connect).
  const onFormKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'Enter' || e.nativeEvent.isComposing || e.defaultPrevented) return;
    const t = e.target;
    if (!(t instanceof HTMLInputElement) || t.type === 'checkbox' || t.type === 'search') return;
    e.preventDefault();
    void primary();
  };

  const locked = busy !== null;

  // One sheet for both steps (no re-mount, so focus and the enter animation stay put).
  const picking = step === 'driver';
  const pickerFooterStart = (
    <Button variant="ghost" icon={<ScanSearch size={14} strokeWidth={1.75} />} onClick={() => void onRescan()} loading={busy === 'rescan'} disabled={locked}>
      {cs('rescan')}
    </Button>
  );
  const pickerFooter = (
    <>
      <Button variant="secondary" onClick={onClose} disabled={locked}>
        {cs('cancel')}
      </Button>
      <Button variant="primary" onClick={() => pickerValue && onPick(pickerValue)} disabled={locked || !pickerValue}>
        {cs('next')}
      </Button>
    </>
  );

  const fields = driver?.fields ?? [];
  const needsPassword = fields.includes('password');
  const passwordKept = !!existing?.hasPassword && !form.password && !form.clearPassword;
  const sshSecretKept = !!existing?.hasSshSecret && !form.clearSshSecret;
  const hasSsh = fields.includes('host');
  const sshHasContent = form.sshEnabled || !!existing?.hasSshSecret;
  const compatFields = fields.filter((k) => COMPAT_FIELDS.has(k) && !TOP_LEVEL.has(k));
  const compatSummary = [
    ...compatFields.filter((k) => !BOOL_FIELDS.has(k)).map((k) => choiceLabel(k, form.fields[k] || fieldDefault(driver, k))),
    ...compatFields.filter((k) => BOOL_FIELDS.has(k) && isOn(form.fields[k] ?? '')).map(fieldLabel),
  ].join(' · ');
  // Probe notes that ask for an explicit server encoding (latin1 default, unsupported pre-4.1
  // charset). The core writes them only with `encoding = auto`; choosing one retires the notice.
  const encodingAuto = (form.fields.encoding || fieldDefault(driver, 'encoding') || 'auto') === 'auto';
  const probeNotes = result?.ok && compatFields.includes('encoding') && encodingAuto ? encodingNotes(result.probe.server.notes) : [];
  const textPlan = result?.ok ? result.probe.server.settings.text_plan : undefined;
  const focusEncoding = () => {
    flushSync(() => setCompatOpen(true));
    document.getElementById('conn-f-encoding')?.focus();
  };

  return (
    <Sheet
      open
      onOpenChange={(o) => !o && onClose()}
      dismissible={!locked}
      width="md"
      title={editing ? cs('titleEdit') : cs('titleNew')}
      description={picking ? cs('pickDriver') : undefined}
      footerStart={
        picking ? pickerFooterStart : (
          <>
            <Tooltip content={cs('testTitle')}>
              <Button variant="secondary" onClick={() => void onTest()} loading={busy === 'test'} disabled={locked}>
                {cs('test')}
              </Button>
            </Tooltip>
            <TestResultLine result={result} drivers={drivers} encodingNote={probeNotes.length > 0} />
          </>
        )
      }
      footer={
        picking ? pickerFooter : (
          <>
            <Button variant="secondary" onClick={onClose} disabled={locked}>
              {cs('cancel')}
            </Button>
            <Button variant={workspaceOpen ? 'primary' : 'secondary'} onClick={() => void onSave()} loading={busy === 'save'} disabled={locked}>
              {cs('save')}
            </Button>
            {!workspaceOpen && (
              <Button variant="primary" onClick={() => void onConnect()} loading={busy === 'connect'} disabled={locked}>
                {cs('connect')}
              </Button>
            )}
          </>
        )
      }
    >
      {picking ? (
        <DriverPicker drivers={drivers} value={pickerValue} onChange={setPickerValue} onPick={onPick} />
      ) : (
        <div className="flex flex-col gap-3" onKeyDown={onFormKey}>
          {error && (
            <Notice tone="danger" onClose={() => setError(null)}>
              {error}
            </Notice>
          )}
          <FormGrid>
            <Label htmlFor="conn-name">{cs('name')}</Label>
            <div className="flex min-w-0 gap-2">
              <Input id="conn-name" value={form.name} placeholder={cs('namePlaceholder')} onChange={(e) => patch({ name: e.target.value })} autoFocus />
              <TagPicker id="conn-tag" className="w-[172px] shrink-0" value={form.tag} onChange={(tag) => patch({ tag })} />
            </div>
  
            <Label htmlFor={driverSelect ? 'conn-driver' : undefined}>{cs('driver')}</Label>
            {driverSelect ? (
              <Select
                id="conn-driver"
                value={form.driver}
                onChange={changeDriver}
                options={drivers.map((d) => ({ value: d.id, label: d.available ? d.name : `${d.name} (${cs('needsInstall')})` }))}
              />
            ) : (
              <div className="flex h-(--h-control) min-w-0 items-center gap-2">
                <DriverBadge driverId={form.driver} name={driver?.name} tag={form.tag} size="sm" />
                <span className="truncate font-medium">{driver?.name ?? form.driver}</span>
                {driver && <span className="shrink-0 text-sm text-fg-secondary">{tierLabel(driver.tier)}</span>}
                <button
                  type="button"
                  className="ml-1 shrink-0 rounded-sm text-sm text-accent hover:underline focus-visible:outline-2 focus-visible:outline-focus"
                  onClick={() => {
                    if (editing) setDriverSelect(true);
                    else {
                      setPickerValue(form.driver);
                      setStep('driver');
                    }
                  }}
                >
                  {cs('change')}
                </button>
              </div>
            )}
  
            {driver && !driver.available && (
              <>
                <span />
                <Notice tone="warning" title={cs('driverUnavailable')}>
                  {driver.detail && <div className="text-sm">{driver.detail}</div>}
                  {driver.installHint && (
                    <div className="text-sm" data-selectable>
                      {driver.installHint}
                    </div>
                  )}
                  {driver.installUrl && (
                    <button
                      type="button"
                      className="mt-1 inline-flex items-center gap-1 text-sm text-accent hover:underline"
                      onClick={() => {
                        const url = driver.installUrl!;
                        setLinkError(null);
                        void openUrl(url).then(setLinkError);
                      }}
                    >
                      <ExternalLink size={12} strokeWidth={1.75} /> {cs('install')}: {driver.installLabel ?? driver.installUrl}
                    </button>
                  )}
                  {linkError && (
                    <Notice tone="danger" className="mt-1" onClose={() => setLinkError(null)}>
                      {linkError}
                    </Notice>
                  )}
                  <div className="mt-1.5">
                    <Button size="sm" variant="secondary" icon={<ScanSearch size={12} />} onClick={() => void onRescan()} loading={busy === 'rescan'} disabled={locked}>
                      {cs('rescan')}
                    </Button>
                  </div>
                </Notice>
              </>
            )}
  
            {existing?.writeAllowed && (
              <>
                <Label>{cs('writeAllowedLabel')}</Label>
                <div className="flex min-h-(--h-control) min-w-0 items-center gap-2">
                  <PenLine size={12} strokeWidth={1.75} className="shrink-0 text-warning" aria-hidden />
                  <span className="min-w-0 flex-1 text-sm text-fg-secondary">{cs('writeAllowed')}</span>
                  <Button
                    size="sm"
                    variant="secondary"
                    className="shrink-0"
                    onClick={() => {
                      void clearWriteAllowed(existing).catch((e: unknown) => setError(errorText(e)));
                    }}
                  >
                    {cs('writeAllowedClear')}
                  </Button>
                </div>
              </>
            )}

            {fields.includes('host') && (
              <>
                <Label htmlFor="conn-host">{cs('host')}</Label>
                <div className="flex min-w-0 items-center gap-2">
                  <Input id="conn-host" value={form.host} placeholder="localhost" onChange={(e) => patch({ host: e.target.value })} />
                  {fields.includes('port') && (
                    <>
                      <label htmlFor="conn-port" className="shrink-0 text-fg-secondary">
                        {cs('port')}
                      </label>
                      <Input
                        id="conn-port"
                        className="w-[76px] shrink-0 tabular-nums"
                        inputMode="numeric"
                        value={form.port}
                        onChange={(e) => patch({ port: e.target.value.replace(/[^\d]/g, '') })}
                      />
                    </>
                  )}
                </div>
              </>
            )}
            {!fields.includes('host') && fields.includes('port') && (
              <>
                <Label htmlFor="conn-port">{cs('port')}</Label>
                <Input id="conn-port" className="w-[76px]" inputMode="numeric" value={form.port} onChange={(e) => patch({ port: e.target.value.replace(/[^\d]/g, '') })} />
              </>
            )}
            {fields.includes('user') && (
              <>
                <Label htmlFor="conn-user">{cs('user')}</Label>
                <Input id="conn-user" value={form.user} onChange={(e) => patch({ user: e.target.value })} />
              </>
            )}
            {needsPassword && (
              <>
                <Label htmlFor="conn-password">{cs('password')}</Label>
                <Input
                  id="conn-password"
                  type="password"
                  autoComplete="new-password"
                  value={form.password}
                  disabled={form.clearPassword}
                  placeholder={passwordKept ? cs('passwordKeep') : undefined}
                  onChange={(e) => patch({ password: e.target.value })}
                />
                <span />
                <div className="flex flex-col gap-0.5">
                  <CheckRow id="conn-savepw" label={cs('savePassword')} checked={form.savePassword} onChange={(savePassword) => patch({ savePassword })} />
                  {existing?.hasPassword && (
                    <CheckRow
                      id="conn-clearpw"
                      label={cs('clearPassword')}
                      checked={form.clearPassword}
                      onChange={(clearPassword) => patch(clearPassword ? { clearPassword, password: '' } : { clearPassword })}
                    />
                  )}
                  <p className="text-sm text-fg-secondary">{form.savePassword ? cs('passwordHint') : cs('savePasswordOff')}</p>
                </div>
              </>
            )}
            {fields.includes('database') && (
              <>
                <Label htmlFor="conn-db">{cs('database')}</Label>
                <Input id="conn-db" value={form.database} onChange={(e) => patch({ database: e.target.value })} />
              </>
            )}
            {fields
              .filter((k) => !TOP_LEVEL.has(k) && !COMPAT_FIELDS.has(k))
              .map((k) => (
                <DriverField key={k} k={k} driver={driver} value={form.fields[k]} error={fieldErrors[k]} onChange={(v) => patchField(k, v)} />
              ))}
            <Label htmlFor="conn-outdir">{cs('outputDir')}</Label>
            <PathInput
              id="conn-outdir"
              value={form.outputDir}
              placeholder={cs('outputDirPlaceholder')}
              onChange={(outputDir) => patch({ outputDir })}
              onBrowse={() => void pickDirectory(form.outputDir).then((p) => p && patch({ outputDir: p }))}
            />
          </FormGrid>
  
          {compatFields.length > 0 && (
            <section className="border-t border-separator pt-3" aria-label={cs('compat')}>
              <FormGrid>
                <div className="flex justify-end">
                  <button
                    type="button"
                    aria-expanded={compatOpen}
                    aria-controls="conn-compat-body"
                    onClick={() => setCompatOpen((o) => !o)}
                    className="inline-flex items-center gap-1 rounded-sm text-fg-secondary hover:text-fg focus-visible:outline-2 focus-visible:outline-focus"
                  >
                    <ChevronRight size={12} strokeWidth={2} className={cn(compatOpen && 'rotate-90')} />
                    {cs('compat')}
                  </button>
                </div>
                <span className="min-w-0 truncate text-sm text-fg-secondary">{compatOpen ? cs('compatHint') : compatSummary}</span>
              </FormGrid>
              {probeNotes.length > 0 && (
                <Notice className="mt-2" title={cs('encodingNoteTitle')}>
                  {probeNotes.map((n, i) => (
                    <div key={i} className="text-sm">
                      {encodingNoteText(n)}
                    </div>
                  ))}
                  {textPlan && <div className="text-sm text-fg-secondary">{cs('encodingNoteTextPlan', { plan: textPlan })}</div>}
                  <Button size="sm" variant="secondary" className="mt-1" onClick={focusEncoding}>
                    {cs('encodingNoteAction')}
                  </Button>
                </Notice>
              )}
              {compatOpen && (
                <div id="conn-compat-body" className="mt-2">
                  <FormGrid>
                    {compatFields.map((k) => (
                      <DriverField key={k} k={k} driver={driver} value={form.fields[k]} error={fieldErrors[k]} onChange={(v) => patchField(k, v)} />
                    ))}
                  </FormGrid>
                </div>
              )}
            </section>
          )}

          {hasSsh && (
            <section className="border-t border-separator pt-3" aria-label={cs('ssh')}>
              <FormGrid>
                <div className="flex justify-end">
                  {sshHasContent ? (
                    <button
                      type="button"
                      aria-expanded={sshOpen}
                      aria-controls="conn-ssh-body"
                      onClick={() => setSshOpen((o) => !o)}
                      className="inline-flex items-center gap-1 rounded-sm text-fg-secondary hover:text-fg focus-visible:outline-2 focus-visible:outline-focus"
                    >
                      <ChevronRight size={12} strokeWidth={2} className={cn(sshOpen && 'rotate-90')} />
                      {cs('ssh')}
                    </button>
                  ) : (
                    <span className="text-fg-secondary">{cs('ssh')}</span>
                  )}
                </div>
                <div className="flex h-(--h-control) min-w-0 items-center gap-2">
                  <Switch
                    id="ssh-enabled"
                    aria-label={cs('sshToggle')}
                    checked={form.sshEnabled}
                    onChange={(sshEnabled) => {
                      patch({ sshEnabled });
                      if (sshEnabled) setSshOpen(true);
                    }}
                  />
                  <label htmlFor="ssh-enabled" className="min-w-0 truncate text-sm text-fg-secondary">
                    {form.sshEnabled && !sshOpen && form.sshHost
                      ? `${form.sshUser ? `${form.sshUser}@` : ''}${form.sshHost}:${form.sshPort || 22}`
                      : cs('sshToggle')}
                  </label>
                </div>
              </FormGrid>
              {sshOpen && sshHasContent && (
                <div id="conn-ssh-body" className="mt-2">
                  <FormGrid>
                    {form.sshEnabled && (
                      <>
                        <Label htmlFor="ssh-host">{cs('sshHost')}</Label>
                        <div className="flex min-w-0 items-center gap-2">
                          <Input id="ssh-host" value={form.sshHost} placeholder="bastion.example.com" onChange={(e) => patch({ sshHost: e.target.value })} />
                          <label htmlFor="ssh-port" className="shrink-0 text-fg-secondary">
                            {cs('port')}
                          </label>
                          <Input
                            id="ssh-port"
                            aria-label={cs('sshPort')}
                            className="w-[76px] shrink-0 tabular-nums"
                            inputMode="numeric"
                            value={form.sshPort}
                            onChange={(e) => patch({ sshPort: e.target.value.replace(/[^\d]/g, '') })}
                          />
                        </div>
                        <Label htmlFor="ssh-user">{cs('sshUser')}</Label>
                        <Input id="ssh-user" value={form.sshUser} onChange={(e) => patch({ sshUser: e.target.value })} />
                        <span className="text-right text-fg-secondary">{cs('sshAuth')}</span>
                        <div>
                          <SegmentedControl<SshAuth>
                            size="md"
                            aria-label={cs('sshAuth')}
                            value={form.sshAuth}
                            onChange={(sshAuth) => patch({ sshAuth })}
                            options={[
                              { value: 'key', label: cs('authKey') },
                              { value: 'password', label: cs('authPassword') },
                              { value: 'agent', label: cs('authAgent') },
                            ]}
                          />
                        </div>
                        {form.sshAuth === 'key' && (
                          <>
                            <Label htmlFor="ssh-key">{cs('sshKeyPath')}</Label>
                            <PathInput
                              id="ssh-key"
                              value={form.sshKeyPath}
                              placeholder="~/.ssh/id_ed25519"
                              onChange={(sshKeyPath) => patch({ sshKeyPath })}
                              onBrowse={() => void pickAnyFile().then((p) => p && patch({ sshKeyPath: p }))}
                            />
                            <Label htmlFor="ssh-pass">{cs('sshPassphrase')}</Label>
                            <Input
                              id="ssh-pass"
                              type="password"
                              autoComplete="new-password"
                              value={form.sshKeyPassphrase}
                              disabled={form.clearSshSecret}
                              placeholder={sshSecretKept ? cs('passwordKeep') : undefined}
                              onChange={(e) => patch({ sshKeyPassphrase: e.target.value })}
                            />
                          </>
                        )}
                        {form.sshAuth === 'password' && (
                          <>
                            <Label htmlFor="ssh-pw">{cs('sshPassword')}</Label>
                            <Input
                              id="ssh-pw"
                              type="password"
                              autoComplete="new-password"
                              value={form.sshPassword}
                              disabled={form.clearSshSecret}
                              placeholder={sshSecretKept ? cs('passwordKeep') : undefined}
                              onChange={(e) => patch({ sshPassword: e.target.value })}
                            />
                          </>
                        )}
                      </>
                    )}
                    <span />
                    <div className="flex flex-col gap-0.5">
                      {form.sshEnabled && (
                        <CheckRow id="ssh-comp" label={cs('sshCompression')} checked={form.sshCompression} onChange={(sshCompression) => patch({ sshCompression })} />
                      )}
                      {/* Offered whenever a secret is stored, even with the tunnel switched off. */}
                      {existing?.hasSshSecret && (
                        <CheckRow
                          id="ssh-clear"
                          label={cs('clearSshSecret')}
                          checked={form.clearSshSecret}
                          onChange={(clearSshSecret) => patch(clearSshSecret ? { clearSshSecret, sshPassword: '', sshKeyPassphrase: '' } : { clearSshSecret })}
                        />
                      )}
                    </div>
                  </FormGrid>
                </div>
              )}
            </section>
          )}
        </div>
      )}
    </Sheet>
  );
}

/** 112px right-aligned labels, 26px controls, 8px gaps. */
function FormGrid({ children }: { children: ReactNode }) {
  return <div className="grid grid-cols-[112px_minmax(0,1fr)] items-center gap-x-3 gap-y-2">{children}</div>;
}

function Label({ htmlFor, children }: { htmlFor?: string; children: ReactNode }) {
  return (
    <label htmlFor={htmlFor} className="truncate text-right text-fg-secondary">
      {children}
    </label>
  );
}

function CheckRow({ id, label, checked, onChange }: { id: string; label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label htmlFor={id} className="flex min-h-[22px] items-center gap-2">
      <Checkbox id={id} checked={checked} onChange={onChange} />
      <span>{label}</span>
    </label>
  );
}

function PathInput({
  id,
  value,
  placeholder,
  onChange,
  onBrowse,
}: {
  id: string;
  value: string;
  placeholder?: string;
  onChange: (v: string) => void;
  onBrowse: () => void;
}) {
  return (
    <div className="flex min-w-0 gap-1.5">
      <Input id={id} value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)} />
      <Button variant="secondary" onClick={onBrowse}>
        {cs('browse')}
      </Button>
    </div>
  );
}

/** The core's `INVALID` message for one field, under its control. */
function FieldError({ id, message }: { id: string; message: string | undefined }) {
  if (!message) return null;
  return (
    <p id={`${id}-error`} role="alert" className="flex items-start gap-1 text-sm text-danger" data-selectable>
      <CircleX size={12} strokeWidth={1.75} className="mt-0.5 shrink-0" />
      <span className="min-w-0">{message}</span>
    </p>
  );
}

/** Registry boolean values (`true`/`1`/`yes`/`on`, like the core's `parse_bool_field`). */
function isOn(value: string): boolean {
  return ['true', '1', 'yes', 'on'].includes(value.trim().toLowerCase());
}

/** One driver-specific field (`sslmode`, `instance`, `encrypt`, `path`, ODBC `driver`, ...). */
function DriverField({
  k,
  driver,
  value,
  error,
  onChange,
}: {
  k: string;
  driver: DriverInfo | undefined;
  value: string | undefined;
  error?: string;
  onChange: (v: string) => void;
}) {
  const id = `conn-f-${k}`;
  const choices = driver?.choices?.[k];
  if ((choices && choices.length > 0) || k === 'sslmode') {
    // Registry choices (label = localized `choice.<field>.<value>`, else the raw value); a saved
    // value outside the list stays selectable as-is, so opening the form never rewrites it.
    const values = choices && choices.length > 0 ? choices : SSLMODES;
    const current = value || fieldDefault(driver, k) || (k === 'sslmode' ? 'prefer' : values[0]);
    const options = values.map((v) => ({ value: v, label: choiceLabel(k, v) }));
    if (current && !values.includes(current)) options.push({ value: current, label: current });
    return (
      <>
        <Label htmlFor={id}>{fieldLabel(k)}</Label>
        <div className="flex min-w-0 flex-col gap-0.5">
          <Select id={id} value={current} onChange={onChange} options={options} />
          <FieldError id={id} message={error} />
        </div>
      </>
    );
  }
  if (BOOL_FIELDS.has(k)) {
    const checked = isOn(value || fieldDefault(driver, k) || 'false');
    const hint = fieldHint(k);
    if (hint) {
      // Long label + warning text (allow_old_password): the switch carries the label itself.
      return (
        <>
          <span />
          <div className="flex min-w-0 flex-col gap-0.5">
            <label htmlFor={id} className="flex min-h-(--h-control) items-center gap-2">
              <Switch id={id} checked={checked} onChange={(v) => onChange(v ? 'true' : 'false')} />
              <span>{fieldLabel(k)}</span>
            </label>
            <p className={cn('flex items-start gap-1 text-sm', checked ? 'text-warning' : 'text-fg-secondary')}>
              {checked && <TriangleAlert size={12} strokeWidth={1.75} className="mt-0.5 shrink-0" />}
              <span className="min-w-0">{hint}</span>
            </p>
            <FieldError id={id} message={error} />
          </div>
        </>
      );
    }
    return (
      <>
        <Label htmlFor={id}>{fieldLabel(k)}</Label>
        <div className="flex min-h-(--h-control) flex-col justify-center gap-0.5">
          <Switch id={id} aria-label={fieldLabel(k)} checked={checked} onChange={(v) => onChange(v ? 'true' : 'false')} />
          <FieldError id={id} message={error} />
        </div>
      </>
    );
  }
  if (k === 'driver' && driver && driver.odbcDrivers.length > 0) {
    return (
      <>
        <Label htmlFor={id}>{cs('odbcDriver')}</Label>
        <Select id={id} value={value || undefined} onChange={onChange} placeholder={cs('odbcDriver')} options={driver.odbcDrivers.map((d) => ({ value: d, label: d }))} />
      </>
    );
  }
  if (k === 'path') {
    return (
      <>
        <Label htmlFor={id}>{fieldLabel(k)}</Label>
        <PathInput
          id={id}
          value={value ?? ''}
          placeholder="C:\data\local.db"
          onChange={onChange}
          onBrowse={() => void pickAnyFile([{ name: 'SQLite', extensions: ['db', 'sqlite', 'sqlite3'] }]).then((p) => p && onChange(p))}
        />
      </>
    );
  }
  return (
    <>
      <Label htmlFor={id}>{fieldLabel(k)}</Label>
      <div className="flex min-w-0 flex-col gap-0.5">
        <Input id={id} value={value ?? ''} onChange={(e) => onChange(e.target.value)} />
        <FieldError id={id} message={error} />
      </div>
    </>
  );
}

function encodingNoteText(n: EncodingNote): string {
  return n.kind === 'latin1Default' ? cs('encodingNote.latin1Default') : cs('encodingNote.unsupportedCharset', { charset: n.charset });
}

/**
 * `✓ PostgreSQL 16.2 · 412ms · SSH 경유 · work_mem 권장 256MB`, or the failure in danger text.
 * `encodingNote`: the probe asks for an explicit server encoding (the notice under 호환성).
 */
function TestResultLine({ result, drivers, encodingNote }: { result: TestResult; drivers: readonly DriverInfo[]; encodingNote: boolean }) {
  if (!result) return null;
  if (result.ok) {
    const p = result.probe;
    const text = [
      cs('testOk', { driver: drivers.find((d) => d.id === p.server.driver)?.name ?? p.server.driver, version: p.server.version, ms: formatDuration(p.elapsedMs) }),
      p.server.settings.protocol?.startsWith('legacy') ? cs('testLegacy') : null,
      encodingNote ? cs('testEncodingNote') : null,
      p.tunnelUsed ? cs('testTunnel') : null,
      p.recommended.workMem ? cs('testWorkMem', { value: p.recommended.workMem }) : null,
    ]
      .filter(Boolean)
      .join(' · ');
    return (
      <Tooltip content={text}>
        <span role="status" className="flex min-w-0 items-center gap-1 text-sm text-fg-secondary">
          <Check size={14} strokeWidth={2} className="shrink-0 text-success" />
          <span className="truncate">{text}</span>
        </span>
      </Tooltip>
    );
  }
  return (
    <Tooltip content={result.error}>
      <span role="alert" className="flex min-w-0 items-center gap-1 text-sm text-danger">
        <CircleX size={14} strokeWidth={1.75} className="shrink-0" />
        <span className="truncate">{cs('testFailed', { error: result.error })}</span>
      </span>
    </Tooltip>
  );
}
