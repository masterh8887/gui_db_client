'use client';

// EXPLAIN result (export panel 계획): the estimate summary (rows labelled 추정 / size / cost /
// time), the tables the plan reads, then the plan tree with its warnings (locate) and the raw
// plan. Plans the backend could not structure (no `root`) show their warnings and the raw EXPLAIN
// text instead.

import type { PlanSummary } from 'ipc-types';
import { FileCode } from 'lucide-react';
import { useState, type ReactNode } from 'react';
import { formatBytes, formatDuration, formatInt } from '@/lib/format';
import { t } from '@/lib/i18n';
import { tableLabel } from '@/lib/utils';
import { Badge, Button } from '@/components/ui';
import { PlanTree, PlanWarningList, RawPlan } from './PlanTree';
import { qs } from './strings';

function Stat({ label, children }: { label: string; children: ReactNode }) {
  return (
    <span className="inline-flex items-baseline gap-1.5 whitespace-nowrap">
      <span className="text-sm text-fg-secondary">{label}</span>
      {children}
    </span>
  );
}

export function PlanSummaryView({ plan }: { plan: PlanSummary }) {
  const [raw, setRaw] = useState(false);
  return (
    <section className="flex flex-col gap-3" aria-label={qs('plan')}>
      <div className="flex flex-wrap items-center gap-x-5 gap-y-1">
        <Stat label={qs('planRows')}>
          <span className="font-medium tabular-nums">{formatInt(plan.estimatedRows)}</span>
          <span className="text-sm text-fg-secondary">{t('estimated')}</span>
        </Stat>
        <Stat label={qs('planBytes')}>
          <span className="font-medium tabular-nums">{formatBytes(plan.estimatedBytes)}</span>
        </Stat>
        {plan.totalCost !== undefined && (
          <Stat label={qs('planCost')}>
            <span className="font-medium tabular-nums">{plan.totalCost.toLocaleString('en-US')}</span>
          </Stat>
        )}
        <span className="text-sm tabular-nums text-fg-tertiary">{formatDuration(plan.elapsedMs)}</span>
        {plan.analyzed && <Badge tone="accent">ANALYZE</Badge>}
        {!plan.root && (
          <Button
            size="sm"
            variant={raw ? 'secondary' : 'ghost'}
            icon={<FileCode size={14} strokeWidth={1.75} />}
            aria-pressed={raw}
            onClick={() => setRaw((v) => !v)}
            className="ml-auto"
          >
            {qs('rawPlan')}
          </Button>
        )}
      </div>
      {plan.tables.length > 0 && (
        <div className="flex flex-wrap items-center gap-1">
          {plan.tables.map((ref, i) => (
            <span
              key={`${ref.database ?? ''}/${tableLabel(ref)}-${i}`}
              className="inline-flex h-[18px] items-center rounded-sm bg-control px-1.5 text-sm text-fg-secondary"
            >
              {tableLabel(ref)}
            </span>
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
