'use client';

// The export panel under the query bar (DESIGN.md §5.D): one of 출력 / 세션 / 계획 / 검사 / 작업,
// chosen by the bar's segments. `bg-panel`, scrolls on its own, height = `ui.exportPanelHeight`
// (at most half of the tab).

import { ListTree } from 'lucide-react';
import { shortcutFor } from '@/lib/keymap';
import { Button, KeyHint } from '@/components/ui';
import { useExtractStore, useQueryDoc } from '@/stores/extract';
import { CheckView } from './CheckView';
import { JobView } from './JobView';
import { OutputOptionsForm } from './OutputOptionsForm';
import { PanelEmpty } from './parts';
import { PlanSummaryView } from './PlanSummaryView';
import { SessionOptionsForm } from './SessionOptionsForm';
import { qs } from './strings';

function PlanView({ tabId }: { tabId: string }) {
  const doc = useQueryDoc(tabId, (d) => ({ lastPlan: d.lastPlan, busy: d.busy }));
  if (!doc) return null;
  if (doc.lastPlan) return <PlanSummaryView plan={doc.lastPlan} />;
  const combo = shortcutFor('query.explain');
  return (
    <PanelEmpty
      title={qs('planEmpty')}
      hint={qs('planEmptyHint')}
      action={
        <Button
          size="sm"
          variant="secondary"
          icon={<ListTree size={14} strokeWidth={1.75} />}
          loading={doc.busy === 'explain'}
          disabled={!!doc.busy && doc.busy !== 'explain'}
          onClick={() => void useExtractStore.getState().explain(tabId)}
        >
          {qs('explain')}
          {combo && <KeyHint combo={combo} />}
        </Button>
      }
    />
  );
}

export function ExportPanel({ tabId, height }: { tabId: string; height: number }) {
  const panel = useQueryDoc(tabId, (d) => d.panel);
  if (!panel) return null;
  return (
    <div
      role="region"
      aria-label={qs('panelSections')}
      data-panel={panel}
      className="list-scroll max-h-[50%] min-h-0 shrink-0 overflow-auto bg-panel px-4 py-3"
      style={{ height }}
    >
      {panel === 'output' ? (
        <OutputOptionsForm tabId={tabId} />
      ) : panel === 'session' ? (
        <SessionOptionsForm tabId={tabId} />
      ) : panel === 'plan' ? (
        <PlanView tabId={tabId} />
      ) : panel === 'check' ? (
        <CheckView tabId={tabId} />
      ) : (
        <JobView tabId={tabId} />
      )}
    </div>
  );
}
