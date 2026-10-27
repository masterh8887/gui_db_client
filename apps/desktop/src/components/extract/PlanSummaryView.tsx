'use client';

// EXPLAIN result: the estimate summary (rows / size / cost / tables), then the plan tree with its
// warnings. Plans the backend could not structure (no `root`) show their warnings and the raw
// EXPLAIN text instead.

import type { PlanSummary } from 'ipc-types';
import { useState } from 'react';
import { formatBytes, formatDuration, formatInt } from '@/lib/format';
import { t } from '@/lib/i18n';
import { tableLabel } from '@/lib/utils';
import { Badge } from '@/components/ui';
import { PlanTree, PlanWarningList, RawPlan } from './PlanTree';

export function PlanSummaryView({ plan }: { plan: PlanSummary }) {
  const [raw, setRaw] = useState(false);
  return (
    <section className="flex flex-col gap-2 rounded border border-border p-2" aria-label={t('extract.plan')}>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-[13px]">
        <span className="text-xs font-semibold text-muted-foreground">{t('extract.plan')}</span>
        <span>
          {t('extract.planRows')}: <b className="tabular-nums">{formatInt(plan.estimatedRows)}</b>{' '}
          <span className="text-[10px] uppercase text-muted-foreground">{t('estimated')}</span>
        </span>
        <span>
          {t('extract.planBytes')}: <b>{formatBytes(plan.estimatedBytes)}</b>
        </span>
        {plan.totalCost !== undefined && (
          <span>
            {t('extract.planCost')}: <b className="tabular-nums">{plan.totalCost.toLocaleString('en-US')}</b>
          </span>
        )}
        <span className="text-muted-foreground">{formatDuration(plan.elapsedMs)}</span>
        {plan.analyzed && <Badge tone="accent">ANALYZE</Badge>}
        {!plan.root && (
          <button className="ml-auto text-xs text-accent hover:underline" aria-pressed={raw} onClick={() => setRaw((v) => !v)}>
            {t('extract.rawPlan')}
          </button>
        )}
      </div>
      {plan.tables.length > 0 && (
        <div className="flex flex-wrap items-center gap-1">
          {plan.tables.map((ref, i) => (
            <Badge key={`${ref.database ?? ''}/${tableLabel(ref)}-${i}`} tone="outline" className="font-mono">
              {tableLabel(ref)}
            </Badge>
          ))}
        </div>
      )}
      {plan.root ? (
        <PlanTree plan={plan} />
      ) : (
        <>
          <PlanWarningList warnings={plan.warnings} />
          {raw && <RawPlan plan={plan} />}
        </>
      )}
    </section>
  );
}
