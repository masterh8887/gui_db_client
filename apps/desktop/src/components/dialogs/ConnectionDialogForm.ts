// Form model of the connection form (DESIGN.md §5.B). Secrets are typed into the form, sent
// once (in `conn_save` or `conn_test`) and cleared right after; this module only shapes data.

import type { Connection, ConnectionInput, DriverInfo, SshAuth } from 'ipc-types';
import { connTag, withConnTag, type EnvTag } from '@/lib/env';
import { toIpcError } from '@/lib/ipc-error';

/** Fields that map to top-level `Connection` members; everything else goes into `fields`. */
export const TOP_LEVEL = new Set(['host', 'port', 'database', 'user', 'password']);
export const BOOL_FIELDS = new Set(['encrypt', 'trust_server_certificate', 'allow_old_password']);
/** PostgreSQL ssl modes; a driver whose registry entry has `choices.sslmode` uses those instead. */
export const SSLMODES = ['disable', 'allow', 'prefer', 'require', 'verify-ca', 'verify-full'];
/**
 * Fields grouped under the collapsible 호환성 / Compatibility section of step 2 (MySQL legacy
 * protocol, server encoding, old passwords; docs/mysql-compat.md §3), so the default form stays
 * short. Any driver that declares one of them gets the section.
 */
export const COMPAT_FIELDS = new Set(['protocol', 'encoding', 'allow_old_password']);

export interface FormState {
  id: string;
  name: string;
  tag: EnvTag | undefined;
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

function driverFieldDefaults(driver: DriverInfo | undefined): Record<string, string> {
  return Object.fromEntries(
    Object.entries(driver?.defaults ?? {})
      .filter(([k]) => !TOP_LEVEL.has(k))
      .map(([k, v]) => [k, v ?? '']),
  );
}

export function emptyForm(driver: DriverInfo | undefined): FormState {
  return {
    id: '',
    name: '',
    tag: undefined,
    driver: driver?.id ?? 'postgres',
    host: '',
    port: driver?.defaults.port ?? '',
    database: '',
    user: '',
    fields: driverFieldDefaults(driver),
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

/**
 * The form of a saved connection. Driver fields it does not have yet (saved before the field
 * existed) show the driver's default, which is also what the core assumes for them.
 */
export function fromConnection(c: Connection, driver?: DriverInfo): FormState {
  const fields: Record<string, string> = driver?.id === c.driver ? driverFieldDefaults(driver) : {};
  for (const [k, v] of Object.entries(c.fields)) if (v !== undefined) fields[k] = v;
  return {
    id: c.id,
    name: c.name,
    tag: connTag(c),
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

/**
 * Switch the form to another driver: its defaults fill the driver fields the user has not typed,
 * and the port follows the new default unless the user typed one of their own.
 */
export function withDriver(f: FormState, next: DriverInfo | undefined, prev: DriverInfo | undefined): FormState {
  if (!next) return f;
  const portWasDefault = f.port === '' || f.port === (prev?.defaults.port ?? '');
  return {
    ...f,
    driver: next.id,
    port: portWasDefault ? (next.defaults.port ?? '') : f.port,
    fields: { ...driverFieldDefaults(next), ...f.fields },
  };
}

/**
 * The `conn_save` input. A secret marked for deletion is never sent along with its flag.
 * Never carries `writeAllowed`: absent keeps the stored flag, and core clears it when the
 * endpoint changes (db-copy.md 2); only the explicit write-allowed actions send it.
 */
export function toInput(f: FormState, existing: Connection | undefined, driver: DriverInfo | undefined): ConnectionInput {
  const fields: Record<string, string> = {};
  for (const k of driver?.fields ?? []) {
    if (TOP_LEVEL.has(k)) continue;
    const v = f.fields[k];
    if (v !== undefined && v !== '') fields[k] = v;
  }
  const port = f.port.trim() === '' ? undefined : Number(f.port);
  const input: ConnectionInput = {
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
    password: f.savePassword && f.password && !f.clearPassword ? f.password : undefined,
    sshPassword: f.sshEnabled && f.sshAuth === 'password' && f.sshPassword && !f.clearSshSecret ? f.sshPassword : undefined,
    sshKeyPassphrase: f.sshEnabled && f.sshAuth === 'key' && f.sshKeyPassphrase && !f.clearSshSecret ? f.sshKeyPassphrase : undefined,
    clearPassword: existing?.hasPassword && f.clearPassword ? true : undefined,
    clearSshSecret: existing?.hasSshSecret && f.clearSshSecret ? true : undefined,
  };
  // Omitting `tag` clears it in core (DESIGN.md §7.2).
  return withConnTag(input, f.tag);
}

/** The form after a save: typed secrets gone, clear requests carried out. */
export function withoutSecrets(f: FormState): FormState {
  return { ...f, password: '', sshPassword: '', sshKeyPassphrase: '', clearPassword: false, clearSshSecret: false };
}

/** The registry value of a field when nothing was chosen (`driver.defaults`, else empty). */
export function fieldDefault(driver: DriverInfo | undefined, k: string): string {
  return driver?.defaults[k] ?? '';
}

/** Whether a compatibility field differs from the driver default (the section then opens). */
export function compatChanged(f: FormState, driver: DriverInfo | undefined): boolean {
  return (driver?.fields ?? []).some((k) => COMPAT_FIELDS.has(k) && (f.fields[k] ?? '') !== '' && f.fields[k] !== fieldDefault(driver, k));
}

/**
 * The field an `INVALID` error from the core names (`mysql field protocol: unknown value 'x'
 * (use auto, modern or legacy)`), so the form can show the message next to that field. `null`
 * for any other error.
 */
export function invalidFieldError(e: unknown, fields: readonly string[]): { field: string; message: string } | null {
  const err = toIpcError(e);
  if (err.code !== 'INVALID') return null;
  const field = /\bfield ([a-z0-9_]+)\s*:/i.exec(err.message)?.[1];
  if (!field || !fields.includes(field)) return null;
  return { field, message: err.detail ? `${err.message} (${err.detail})` : err.message };
}
