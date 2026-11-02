'use client';

// The 28px query bar under the editor (DESIGN.md §5.D), left to right: the SQL source menu
// (`SQL ▾` / `파일 ▾`), the export panel segments `출력 · 세션 │ 계획 · 검사 · 작업` with their
// badges, the output summary, the busy caption and the run group `[검사] [계획] [▶ 실행 ⌘↵]`.
// While the tab's last job is active 실행 becomes `■ 중지`, which only asks (`askCancel`); the
// inline confirm follows `ui.cancelConfirmJobId`. Rust decides, the buttons follow `job:*`.

import type { JobState } from 'ipc-types';
import { Check, ChevronDown, ListTree, Play, ShieldCheck, Square } from 'lucide-react';
import type { ReactNode } from 'react';
import { pickSqlFile } from '@/lib/dialogs';
import { formatCompact, joinPath } from '@/lib/format';
import { askCancel, cancelJob } from '@/lib/job-actions';
import { comboLabel, shortcutFor } from '@/lib/keymap';
import { useIsMac } from '@/lib/platform';
import { cn } from '@/lib/utils';
import { Button, DropdownMenu, Kbd, SegmentedControl, Spinner, StatusBar, Tooltip, type MenuItem } from '@/components/ui';
import { csvEncoding, defaultCsvOpts, defaultParquetOpts, useExtractStore, useQueryDoc, type ExportPanelSection, type QueryDoc } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { middleTruncate } from './parts';
import { qs } from './strings';

/** Job states during which the run button is a stop button (DESIGN.md §5.D). */
export const STOPPABLE_STATES: readonly JobState[] = ['pending', 'preflight', 'running', 'finishing'];

export function isStoppable(state: JobState | undefined): boolean {
  return state !== undefined && STOPPABLE_STATES.includes(state);
}

/**
 * `CSV · BOM · gzip` / `CSV · EUC-KR`, and the full output path (folder + pattern + extension).
 * The panel starts collapsed, so this is the visible description of the file: it names a
 * non-UTF-8 encoding and shows BOM only when one is written (UTF-8 only, `CsvOpts::writes_bom`).
 */
export function outputSummary(output: QueryDoc['output']): { text: string; path: string } {
  const parts: string[] = [];
  let ext: string;
  if (output.format === 'csv') {
    const csv = output.csv ?? defaultCsvOpts();
    parts.push('CSV');
    if (csvEncoding(csv) === 'euc-kr') parts.push('EUC-KR');
    else if (csv.bom) parts.push('BOM');
    if (output.compress) parts.push(output.compress);
    ext = `.csv${output.compress === 'gzip' ? '.gz' : output.compress === 'zstd' ? '.zst' : ''}`;
  } else {
    const pq = output.parquet ?? defaultParquetOpts();
    parts.push('Parquet');
    if (pq.compression !== 'none') parts.push(pq.compression);
    ext = '.parquet';
  }
  if (output.split?.rows) parts.push(`${formatCompact(output.split.rows)}${qs('rowsUnit')}`);
  else if (output.split?.megabytes) parts.push(`${output.split.megabytes} MB`);
  return { text: parts.join(' · '), path: joinPath(output.dir, `${output.namePattern}${ext}`) };
}

function busyCaption(busy: QueryDoc['busy']): string | null {
  switch (busy) {
    case 'check':
      return qs('busyCheck');
    case 'explain':
      return qs('busyExplain');
    case 'preflight':
    case 'starting':
      return qs('busyStart');
    case 'file':
    case 'select':
      return qs('busyLoad');
    default:
      return null;
  }
}

function Dot({ tone, label }: { tone: 'warning' | 'danger'; label: string }) {
  return <span role="img" aria-label={label} className={cn('inline-block size-1.5 rounded-full', tone === 'warning' ? 'bg-warning' : 'bg-danger')} />;
}

/**
 * A bar button with a tooltip `label (KeyHint)`; the label hides below 560px of tab width
 * (DESIGN.md §5.D: 검사 / 계획 go icon-only), or below 420px with `keepLabel` (실행 / 중지, the
 * bar's main action, stays readable).
 */
function BarButton({
  label,
  combo,
  icon,
  variant,
  loading,
  disabled,
  onClick,
  kbd,
  keepLabel = false,
}: {
  label: string;
  combo?: string;
  icon: ReactNode;
  variant: 'primary' | 'secondary' | 'danger';
  loading?: boolean;
  disabled?: boolean;
  onClick: () => void;
  kbd?: ReactNode;
  keepLabel?: boolean;
}) {
  const mac = useIsMac();
  const tip = combo ? `${label} (${comboLabel(combo, mac)})` : label;
  return (
    <Tooltip content={tip} delay={300} side="top">
      <Button size="sm" variant={variant} icon={icon} loading={loading} disabled={disabled} onClick={onClick} aria-label={label}>
        <span className={keepLabel ? '@max-[420px]:hidden' : '@max-[560px]:hidden'}>{label}</span>
        {kbd}
      </Button>
    </Tooltip>
  );
}

export function QueryBar({ tabId }: { tabId: string }) {
  const mac = useIsMac();
  const doc = useQueryDoc(tabId, (d) => ({
    sqlMode: d.sqlMode,
    busy: d.busy,
    panel: d.panel,
    panelCollapsed: d.panelCollapsed,
    lastJobId: d.lastJobId,
    lastPlan: d.lastPlan,
    lastCheck: d.lastCheck,
    output: d.output,
  }));
  const jobState = useJobsStore((s) => (doc?.lastJobId ? s.jobs[doc.lastJobId]?.summary.state : undefined));
  const confirming = useUiStore((s) => !!doc?.lastJobId && s.cancelConfirmJobId === doc.lastJobId);
  if (!doc) return null;

  const ex = useExtractStore.getState;
  const { busy, lastJobId } = doc;
  const stoppable = isStoppable(jobState) && !!lastJobId;
  const caption = busyCaption(busy);
  const summary = outputSummary(doc.output);
  const runCombo = shortcutFor('query.run') ?? 'mod+enter';

  const openFile = async () => {
    const path = await pickSqlFile();
    if (path) await ex().loadSqlFile(tabId, path);
  };

  const modeItems: MenuItem[] = [
    { id: 'text', kind: 'radio', checked: doc.sqlMode === 'text', label: qs('modeText'), onSelect: () => ex().setSqlMode(tabId, 'text') },
    { id: 'file', kind: 'radio', checked: doc.sqlMode === 'file', label: qs('modeFileItem'), onSelect: () => ex().setSqlMode(tabId, 'file') },
    { id: 'open', label: qs('openFile'), shortcut: shortcutFor('query.openFile'), separatorBefore: true, onSelect: () => void openFile() },
    {
      id: 'other',
      label: qs('openInConnection'),
      separatorBefore: true,
      onSelect: () => useUiStore.getState().openPalette('connections', '', { kind: 'openInConnection', tabId }),
    },
  ];

  const planWarned = (doc.lastPlan?.warnings.length ?? 0) > 0;
  const checkBadge = doc.lastCheck ? (
    doc.lastCheck.ok ? (
      <Check size={11} strokeWidth={2.25} className="text-success" role="img" aria-label={qs('badgeCheckOk')} />
    ) : (
      <Dot tone="danger" label={qs('badgeCheckFailed')} />
    )
  ) : null;
  const jobBadge =
    jobState === 'warned' ? (
      <Dot tone="warning" label={qs('badgeJobWarned')} />
    ) : isStoppable(jobState) ? (
      <span role="img" aria-label={qs('badgeJobActive')} className="inline-flex">
        <Spinner size={10} />
      </span>
    ) : null;

  const onSection = (v: ExportPanelSection) => ex().setPanel(tabId, v);
  // Clicking the active segment toggles the panel (a different one was opened by onChange).
  const onActivate = (v: ExportPanelSection) => {
    if (v === doc.panel) ex().togglePanel(tabId);
  };

  return (
    <StatusBar className="relative">
      <div role="toolbar" aria-label={qs('queryBar')} className="flex h-full min-w-0 flex-1 items-center gap-2">
        <DropdownMenu items={modeItems} align="start" side="top">
          <Button size="sm" variant="ghost" aria-label={`${qs('modeMenu')}: ${doc.sqlMode === 'text' ? qs('modeSql') : qs('modeFile')}`} className="-ml-1">
            {doc.sqlMode === 'text' ? qs('modeSql') : qs('modeFile')}
            <ChevronDown size={12} strokeWidth={1.75} className="text-fg-secondary" />
          </Button>
        </DropdownMenu>
        <span aria-hidden className="h-3.5 w-px shrink-0 bg-separator-strong" />
        <SegmentedControl<ExportPanelSection>
          variant="tabs"
          size="sm"
          aria-label={qs('panelSections')}
          value={doc.panel}
          onChange={onSection}
          onActivate={onActivate}
          options={[
            { value: 'output', label: qs('segOutput') },
            { value: 'session', label: qs('segSession') },
            { value: 'plan', label: qs('segPlan'), separatorBefore: true, badge: planWarned ? <Dot tone="warning" label={qs('badgePlanWarnings')} /> : null },
            { value: 'check', label: qs('segCheck'), badge: checkBadge },
            { value: 'job', label: qs('segJob'), badge: jobBadge },
          ]}
        />
        <Tooltip content={qs('outputTip', { path: summary.path })} side="top">
          <button
            type="button"
            onClick={() => onSection('output')}
            className="min-w-0 truncate rounded-sm px-1 text-left text-sm text-fg-secondary hover:text-fg @max-[760px]:hidden"
            data-selectable
          >
            {summary.text}
            <span className="text-fg-tertiary"> → </span>
            {middleTruncate(summary.path, 56)}
          </button>
        </Tooltip>
        <span className="ml-auto" />
        {caption && (
          <span role="status" className="shrink-0 whitespace-nowrap text-sm text-fg-secondary">
            {caption}
          </span>
        )}
      </div>

      {stoppable && confirming && lastJobId ? (
        <div className="flex shrink-0 items-center gap-1.5" role="group" aria-label={qs('cancelAsk')}>
          <span className="text-sm">{qs('cancelAsk')}</span>
          <Button size="sm" variant="dangerFilled" onClick={() => void cancelJob(lastJobId)}>
            {qs('yes')}
          </Button>
          <Button size="sm" variant="secondary" onClick={() => useUiStore.getState().setCancelConfirm(null)}>
            {qs('no')}
          </Button>
        </div>
      ) : (
        <div className="flex shrink-0 items-center gap-1.5">
          <BarButton
            label={qs('check')}
            combo={shortcutFor('query.check')}
            icon={<ShieldCheck size={14} strokeWidth={1.75} />}
            variant="secondary"
            loading={busy === 'check'}
            disabled={!!busy && busy !== 'check'}
            onClick={() => void ex().check(tabId)}
          />
          <BarButton
            label={qs('explain')}
            combo={shortcutFor('query.explain')}
            icon={<ListTree size={14} strokeWidth={1.75} />}
            variant="secondary"
            loading={busy === 'explain'}
            disabled={!!busy && busy !== 'explain'}
            onClick={() => void ex().explain(tabId)}
          />
          {stoppable && lastJobId ? (
            <BarButton
              label={qs('stop')}
              combo="mod+."
              icon={<Square size={11} strokeWidth={2} fill="currentColor" />}
              variant="danger"
              keepLabel
              onClick={() => askCancel(lastJobId)}
            />
          ) : (
            <BarButton
              label={qs('run')}
              combo={runCombo}
              icon={<Play size={12} strokeWidth={2} fill="currentColor" />}
              variant="primary"
              keepLabel
              loading={busy === 'preflight' || busy === 'starting'}
              disabled={!!busy && busy !== 'preflight' && busy !== 'starting'}
              onClick={() => void ex().run(tabId)}
              kbd={
                <Kbd inverse className="@max-[560px]:hidden">
                  {comboLabel(runCombo, mac)}
                </Kbd>
              }
            />
          )}
        </div>
      )}
    </StatusBar>
  );
}
