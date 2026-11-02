'use client';

// Export panel 세션 (DESIGN.md §5.D): the five session options. Options the driver does not
// support (`DriverInfo.sessionOpts`) stay visible but disabled, with the reason as a tooltip.

import type { DriverInfo, SessionOpts } from 'ipc-types';
import { useId } from 'react';
import { num } from '@/lib/format';
import { cn } from '@/lib/utils';
import { Input, NumberInput } from '@/components/ui';
import { selectDriver, useConnectionsStore } from '@/stores/connections';
import { useExtractStore, useQueryDoc } from '@/stores/extract';
import { FormRow } from './parts';
import { qs } from './strings';

type SessionKey = keyof SessionOpts;

/** Which form rows a registry `session_opts` entry enables. */
const OPT_KEYS: Record<string, SessionKey> = {
  work_mem: 'workMem',
  statement_timeout: 'statementTimeoutMs',
  max_execution_time: 'statementTimeoutMs',
  lock_timeout: 'lockTimeoutMs',
  max_parallel_workers_per_gather: 'maxParallelWorkers',
  fetch_batch: 'fetchBatchRows',
};

const ALL_KEYS: readonly SessionKey[] = ['workMem', 'statementTimeoutMs', 'lockTimeoutMs', 'maxParallelWorkers', 'fetchBatchRows'];

/** Session keys the driver supports (all of them while the driver is unknown). */
export function supportedSessionKeys(driver: DriverInfo | undefined): Set<SessionKey> {
  if (!driver) return new Set(ALL_KEYS);
  return new Set(driver.sessionOpts.map((o) => OPT_KEYS[o]).filter((k): k is SessionKey => !!k));
}

/** work_mem values offered as one-click hints. */
export const WORK_MEM_HINTS = ['64MB', '256MB', '1GB'] as const;

export function SessionOptionsForm({ tabId }: { tabId: string }) {
  const uid = useId();
  const id = (k: string) => `${uid}-${k}`;
  const doc = useQueryDoc(tabId, (d) => ({ session: d.session, connId: d.connId }));
  const driver = useConnectionsStore((s) => selectDriver(s, s.connections.find((c) => c.id === doc?.connId)?.driver));
  if (!doc) return null;
  const { session } = doc;
  const keys = supportedSessionKeys(driver);
  const set = (p: Partial<SessionOpts>) => useExtractStore.getState().setSession(tabId, p);
  const off = (k: SessionKey) => !keys.has(k);
  const why = (k: SessionKey) => (off(k) ? qs('unsupported', { driver: driver?.name ?? '' }) : undefined);

  return (
    <div className="flex max-w-[560px] flex-col gap-2" role="group" aria-label={qs('session')}>
      <FormRow
        label={qs('workMem')}
        htmlFor={id('workmem')}
        title={why('workMem')}
        hint={
          off('workMem') ? undefined : (
            <span className="flex items-center gap-1">
              <span className="mr-1">{qs('workMemHints')}</span>
              {WORK_MEM_HINTS.map((v) => (
                <button
                  key={v}
                  type="button"
                  onClick={() => set({ workMem: v })}
                  className={cn(
                    'h-[18px] rounded-sm px-1.5 text-xs tabular-nums hover:bg-hover',
                    session.workMem === v ? 'bg-control font-medium text-fg' : 'text-accent',
                  )}
                >
                  {v}
                </button>
              ))}
            </span>
          )
        }
      >
        <Input
          id={id('workmem')}
          className="font-mono"
          value={session.workMem ?? ''}
          placeholder="64MB"
          disabled={off('workMem')}
          onChange={(e) => set({ workMem: e.target.value || undefined })}
        />
      </FormRow>
      <FormRow label={qs('statementTimeout')} htmlFor={id('stmt')} title={why('statementTimeoutMs')}>
        <NumberInput
          id={id('stmt')}
          min={0}
          disabled={off('statementTimeoutMs')}
          value={num(session.statementTimeoutMs)}
          onChange={(v) => set({ statementTimeoutMs: v === undefined ? undefined : BigInt(v) })}
        />
      </FormRow>
      <FormRow label={qs('lockTimeout')} htmlFor={id('lock')} title={why('lockTimeoutMs')}>
        <NumberInput
          id={id('lock')}
          min={0}
          disabled={off('lockTimeoutMs')}
          value={num(session.lockTimeoutMs)}
          onChange={(v) => set({ lockTimeoutMs: v === undefined ? undefined : BigInt(v) })}
        />
      </FormRow>
      <FormRow label={qs('maxParallel')} htmlFor={id('par')} title={why('maxParallelWorkers')}>
        <NumberInput
          id={id('par')}
          min={0}
          max={64}
          disabled={off('maxParallelWorkers')}
          value={session.maxParallelWorkers ?? undefined}
          onChange={(v) => set({ maxParallelWorkers: v })}
        />
      </FormRow>
      <FormRow label={qs('fetchBatch')} htmlFor={id('fetch')} title={why('fetchBatchRows')}>
        <NumberInput
          id={id('fetch')}
          min={1}
          max={65536}
          disabled={off('fetchBatchRows')}
          value={session.fetchBatchRows ?? undefined}
          onChange={(v) => set({ fetchBatchRows: v })}
        />
      </FormRow>
    </div>
  );
}
