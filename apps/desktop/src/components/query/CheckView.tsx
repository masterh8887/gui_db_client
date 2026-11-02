'use client';

// Export panel 검사 (DESIGN.md §5.D): "쿼리 정상 · {KIND}" or the issues with their code chips.

import { CircleCheck, CircleX, ShieldCheck } from 'lucide-react';
import { shortcutFor } from '@/lib/keymap';
import { Button, KeyHint } from '@/components/ui';
import { useExtractStore, useQueryDoc } from '@/stores/extract';
import { CodeChip, PanelEmpty } from './parts';
import { qs } from './strings';

export function CheckView({ tabId }: { tabId: string }) {
  const doc = useQueryDoc(tabId, (d) => ({ lastCheck: d.lastCheck, busy: d.busy }));
  if (!doc) return null;
  const { lastCheck, busy } = doc;
  const combo = shortcutFor('query.check');

  if (!lastCheck) {
    return (
      <PanelEmpty
        title={qs('checkEmpty')}
        hint={qs('checkEmptyHint')}
        action={
          <Button
            size="sm"
            variant="secondary"
            icon={<ShieldCheck size={14} strokeWidth={1.75} />}
            loading={busy === 'check'}
            disabled={!!busy && busy !== 'check'}
            onClick={() => void useExtractStore.getState().check(tabId)}
          >
            {qs('check')}
            {combo && <KeyHint combo={combo} />}
          </Button>
        }
      />
    );
  }

  if (lastCheck.ok) {
    return (
      <div role="status" className="flex items-center gap-2 text-base">
        <CircleCheck size={14} strokeWidth={1.75} className="shrink-0 text-success" />
        <span>{qs('checkOk', { kind: lastCheck.kind.toUpperCase() })}</span>
      </div>
    );
  }

  return (
    <section className="flex flex-col gap-2" aria-label={qs('checkIssues')}>
      <div className="flex items-center gap-2 text-base">
        <CircleX size={14} strokeWidth={1.75} className="shrink-0 text-danger" />
        <span className="font-medium">{qs('checkIssues')}</span>
        {lastCheck.statements > 1 && <span className="text-sm text-fg-secondary">{qs('statements', { n: lastCheck.statements })}</span>}
      </div>
      <ul className="flex flex-col">
        {lastCheck.issues.map((i, n) => (
          <li key={`${i.code}-${n}`} className="flex items-start gap-2 border-t border-separator py-1.5 first:border-t-0">
            <CodeChip className="mt-px">{i.code}</CodeChip>
            <div className="min-w-0 flex-1">
              <div>{i.message}</div>
              {i.detail && (
                <div className="text-sm text-fg-secondary" data-selectable>
                  {i.detail}
                </div>
              )}
              {i.recommendation && <div className="text-sm text-fg-secondary">{i.recommendation}</div>}
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}
