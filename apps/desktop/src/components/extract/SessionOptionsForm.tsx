'use client';

// Session options, filtered by what the driver supports (`DriverInfo.sessionOpts`).

import type { DriverInfo } from 'ipc-types';
import { num } from '@/lib/format';
import { t } from '@/lib/i18n';
import { Field, Input, NumberInput } from '@/components/ui';
import { useExtractStore } from '@/stores/extract';

/** Which form rows a registry `session_opts` entry enables. */
const OPT_KEYS: Record<string, 'workMem' | 'statementTimeoutMs' | 'lockTimeoutMs' | 'maxParallelWorkers' | 'fetchBatchRows'> = {
  work_mem: 'workMem',
  statement_timeout: 'statementTimeoutMs',
  max_execution_time: 'statementTimeoutMs',
  lock_timeout: 'lockTimeoutMs',
  max_parallel_workers_per_gather: 'maxParallelWorkers',
  fetch_batch: 'fetchBatchRows',
};

export function supportedSessionKeys(driver: DriverInfo | undefined): Set<string> {
  if (!driver) return new Set(Object.values(OPT_KEYS));
  return new Set(driver.sessionOpts.map((o) => OPT_KEYS[o]).filter(Boolean));
}

export function SessionOptionsForm({ driver }: { driver: DriverInfo | undefined }) {
  const session = useExtractStore((s) => s.session);
  const setSession = useExtractStore((s) => s.setSession);
  const keys = supportedSessionKeys(driver);

  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="mb-1 text-xs font-semibold text-muted-foreground">{t('extract.session')}</legend>
      {keys.has('workMem') && (
        <Field label={t('extract.workMem')} htmlFor="sess-workmem" hint="64MB, 256MB, 1GB">
          <Input id="sess-workmem" value={session.workMem ?? ''} placeholder="64MB" onChange={(e) => setSession({ workMem: e.target.value || undefined })} />
        </Field>
      )}
      {keys.has('statementTimeoutMs') && (
        <Field label={t('extract.statementTimeout')} htmlFor="sess-stmt">
          <NumberInput id="sess-stmt" min={0} value={num(session.statementTimeoutMs)} onChange={(v) => setSession({ statementTimeoutMs: v === undefined ? undefined : BigInt(v) })} />
        </Field>
      )}
      {keys.has('lockTimeoutMs') && (
        <Field label={t('extract.lockTimeout')} htmlFor="sess-lock">
          <NumberInput id="sess-lock" min={0} value={num(session.lockTimeoutMs)} onChange={(v) => setSession({ lockTimeoutMs: v === undefined ? undefined : BigInt(v) })} />
        </Field>
      )}
      {keys.has('maxParallelWorkers') && (
        <Field label={t('extract.maxParallel')} htmlFor="sess-par">
          <NumberInput id="sess-par" min={0} max={64} value={session.maxParallelWorkers ?? undefined} onChange={(v) => setSession({ maxParallelWorkers: v })} />
        </Field>
      )}
      {keys.has('fetchBatchRows') && (
        <Field label={t('extract.fetchBatch')} htmlFor="sess-fetch">
          <NumberInput id="sess-fetch" min={1} max={65536} value={session.fetchBatchRows ?? undefined} onChange={(v) => setSession({ fetchBatchRows: v })} />
        </Field>
      )}
    </fieldset>
  );
}
