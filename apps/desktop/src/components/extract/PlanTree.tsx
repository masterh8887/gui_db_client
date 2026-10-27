'use client';

// Collapsible EXPLAIN tree for `PlanSummary.root`: per node the type, relation / alias, estimated
// rows x width, total cost with its share of the plan's cost, actual rows / loops / time for
// EXPLAIN ANALYZE, and the `extra` entries as chips. Plan warnings are listed on top and marked on
// the nodes they name (see `matchWarnings`). The raw EXPLAIN output is one toggle away.

import type { PlanNode, PlanSummary, PlanWarning, Severity } from 'ipc-types';
import { ChevronDown, ChevronRight, ChevronsDownUp, ChevronsUpDown, Crosshair, FileCode, Table2 } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { formatDuration, formatInt } from '@/lib/format';
import { getLang, t } from '@/lib/i18n';
import { cn } from '@/lib/utils';
import { Badge, Button, Notice } from '@/components/ui';
import {
  ROOT_ID,
  ancestorIds,
  childId,
  costShare,
  countNodes,
  matchWarnings,
  parentIds,
  planTotalCost,
  relationLabel,
  worstSeverity,
  type WarningMatches,
} from './planModel';

// Strings that `lib/i18n` does not have (kept local so this panel owns its text).
const TEXT = {
  ko: {
    tree: '실행 계획 트리',
    expandAll: '모두 펼치기',
    collapseAll: '모두 접기',
    expand: '펼치기',
    collapse: '접기',
    nodes: '노드 {n}개',
    estimate: '예상',
    actual: '실제',
    rowsUnit: '행',
    loops: '{n}회',
    cost: '비용',
    costShare: '전체 비용의 {pct}',
    startup: '시작 비용 {cost}',
    locate: '노드 보기',
    misestimate: '예상과 실제 행 수가 10배 이상 다릅니다',
    rawFormat: '형식: {format}',
  },
  en: {
    tree: 'Plan tree',
    expandAll: 'Expand all',
    collapseAll: 'Collapse all',
    expand: 'Expand',
    collapse: 'Collapse',
    nodes: '{n} nodes',
    estimate: 'est.',
    actual: 'actual',
    rowsUnit: ' rows',
    loops: '{n} loops',
    cost: 'cost',
    costShare: '{pct} of the total cost',
    startup: 'startup cost {cost}',
    locate: 'Show node',
    misestimate: 'Actual rows differ from the estimate by 10x or more',
    rawFormat: 'format: {format}',
  },
} as const;

type TextKey = keyof (typeof TEXT)['en'];

function tx(key: TextKey, params?: Record<string, string | number>): string {
  let s: string = TEXT[getLang()][key] ?? TEXT.en[key];
  if (params) for (const [k, v] of Object.entries(params)) s = s.split(`{${k}}`).join(String(v));
  return s;
}

const NOTICE_TONE: Record<Severity, 'info' | 'warning' | 'danger'> = { info: 'info', warning: 'warning', danger: 'danger' };
const BADGE_TONE: Record<Severity, 'accent' | 'warning' | 'danger'> = { info: 'accent', warning: 'warning', danger: 'danger' };
const ROW_TONE: Record<Severity, string> = {
  info: 'border-l-accent bg-accent/5',
  warning: 'border-l-warning bg-warning/10',
  danger: 'border-l-danger bg-danger/10',
};

const EMPTY: ReadonlySet<string> = new Set();

// ---- formatting ----------------------------------------------------------------------------

function formatCost(n: number): string {
  return n.toLocaleString('en-US', { maximumFractionDigits: 2 });
}

function formatShare(share: number): string {
  if (share > 0 && share < 0.001) return '<0.1%';
  return `${(share * 100).toFixed(share < 0.1 ? 1 : 0)}%`;
}

function formatMs(ms: number): string {
  if (ms < 1) return `${ms.toFixed(3)} ms`;
  if (ms < 1000) return `${ms.toFixed(ms < 10 ? 2 : 1)} ms`;
  return formatDuration(ms);
}

/** Accessible name of a node: its type plus relation (`Seq Scan public.orders`). */
export function planNodeLabel(node: PlanNode): string {
  const rel = relationLabel(node);
  return rel ? `${node.nodeType} ${rel}` : node.nodeType;
}

// ---- warnings ------------------------------------------------------------------------------

export interface PlanWarningListProps {
  warnings: readonly PlanWarning[];
  /** Node ids per warning (same order); a warning with nodes gets a "show node" button. */
  nodesOf?: readonly (readonly string[])[];
  onLocate?: (ids: readonly string[]) => void;
}

/** The plan warnings as notices; "no plan warnings" when there are none. */
export function PlanWarningList({ warnings, nodesOf, onLocate }: PlanWarningListProps) {
  if (warnings.length === 0) return <div className="text-xs text-muted-foreground">{t('extract.planNoWarnings')}</div>;
  return (
    <div className="flex flex-col gap-1.5" aria-label={t('extract.planWarnings')}>
      {warnings.map((w, i) => {
        const ids = nodesOf?.[i] ?? [];
        return (
          <Notice key={`${w.code}-${i}`} tone={NOTICE_TONE[w.severity]} title={w.code}>
            <div className="flex items-start gap-2">
              <div className="min-w-0 flex-1">
                {w.message}
                {w.detail && <div className="text-xs text-muted-foreground">{w.detail}</div>}
              </div>
              {ids.length > 0 && onLocate && (
                <Button size="sm" variant="ghost" icon={<Crosshair size={12} />} onClick={() => onLocate(ids)} className="shrink-0">
                  {tx('locate')}
                </Button>
              )}
            </div>
          </Notice>
        );
      })}
    </div>
  );
}

// ---- tree ----------------------------------------------------------------------------------

interface TreeCtx {
  collapsed: ReadonlySet<string>;
  toggle: (id: string) => void;
  total: number | undefined;
  analyzed: boolean;
  matches: WarningMatches;
}

function Metric({ label, children, title, className }: { label: string; children: ReactNode; title?: string; className?: string }) {
  return (
    <span className={cn('whitespace-nowrap text-xs tabular-nums', className)} title={title}>
      <span className="text-muted-foreground">{label}</span> {children}
    </span>
  );
}

function ExtraChips({ extra }: { extra: PlanNode['extra'] }) {
  const entries = Object.entries(extra).filter((e): e is [string, string] => typeof e[1] === 'string' && e[1].length > 0);
  if (entries.length === 0) return null;
  return (
    <div className="mt-1 flex flex-wrap gap-1">
      {entries.map(([k, v]) => (
        <span
          key={k}
          data-extra={k}
          className="inline-flex max-w-full items-baseline gap-1 rounded-sm border border-border bg-muted px-1.5 text-[11px] leading-[18px]"
        >
          <span className="shrink-0 text-muted-foreground">{k}</span>
          <span className="max-w-[28rem] truncate font-mono" title={v}>
            {v}
          </span>
        </span>
      ))}
    </div>
  );
}

function CostBar({ share }: { share: number }) {
  const label = tx('costShare', { pct: formatShare(share) });
  return (
    <div role="img" aria-label={label} title={label} className="mt-1 h-[3px] w-full max-w-72 overflow-hidden rounded-full bg-border/70">
      <div className="h-full rounded-full bg-accent/70" style={{ width: `${Math.max(share * 100, share > 0 ? 1 : 0)}%` }} />
    </div>
  );
}

function PlanNodeItem({ node, id, depth, ctx }: { node: PlanNode; id: string; depth: number; ctx: TreeCtx }) {
  const hasChildren = node.children.length > 0;
  const expanded = hasChildren && !ctx.collapsed.has(id);
  const warnings = ctx.matches.byNode.get(id) ?? [];
  const severity = worstSeverity(warnings);
  const share = costShare(node, ctx.total);
  const rel = relationLabel(node);
  const label = planNodeLabel(node);
  const showAlias = node.alias && node.alias !== node.relation?.name;
  const hasActuals = ctx.analyzed && (node.actualRows !== undefined || node.actualLoops !== undefined || node.actualTimeMs !== undefined);
  const misestimate =
    hasActuals && node.actualRows !== undefined && node.planRows !== undefined && Math.max(node.actualRows, node.planRows) >= 100
      ? Math.max(node.actualRows, 1) / Math.max(node.planRows, 1) >= 10 || Math.max(node.planRows, 1) / Math.max(node.actualRows, 1) >= 10
      : false;

  return (
    <li
      role="treeitem"
      aria-level={depth + 1}
      aria-expanded={hasChildren ? expanded : undefined}
      aria-selected={false}
      aria-label={label}
      data-node-id={id}
      data-warned={severity}
    >
      <div
        tabIndex={-1}
        className={cn(
          'flex min-h-7 items-start gap-1 rounded-sm border-l-2 border-l-transparent py-1 pr-2 outline-none focus-visible:ring-1 focus-visible:ring-accent',
          severity && ROW_TONE[severity],
        )}
        style={{ paddingLeft: depth * 16 + 2 }}
      >
        {hasChildren ? (
          <button
            type="button"
            className="mt-0.5 inline-flex h-4 w-4 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-muted hover:text-foreground"
            aria-label={`${expanded ? tx('collapse') : tx('expand')} – ${label}`}
            onClick={() => ctx.toggle(id)}
          >
            {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          </button>
        ) : (
          <span className="h-4 w-4 shrink-0" aria-hidden />
        )}
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-[13px] leading-5">
            <span className="font-semibold">{node.nodeType}</span>
            {node.parentRelationship && <span className="text-[11px] text-muted-foreground">{node.parentRelationship}</span>}
            {rel && (
              <span className="inline-flex items-baseline gap-1 font-mono text-xs">
                <Table2 size={11} className="self-center text-muted-foreground" aria-hidden />
                {rel}
                {showAlias && <span className="text-muted-foreground">{node.alias}</span>}
              </span>
            )}
            {(node.planRows !== undefined || node.planWidth !== undefined) && (
              <Metric label={tx('estimate')}>
                {node.planRows !== undefined && (
                  <>
                    {formatInt(node.planRows)}
                    {tx('rowsUnit')}
                  </>
                )}
                {node.planWidth !== undefined && <span className="text-muted-foreground"> × {node.planWidth} B</span>}
              </Metric>
            )}
            {node.totalCost !== undefined && (
              <Metric label={tx('cost')} title={node.startupCost !== undefined ? tx('startup', { cost: formatCost(node.startupCost) }) : undefined}>
                {formatCost(node.totalCost)}
                {share !== undefined && <span className="text-muted-foreground"> ({formatShare(share)})</span>}
              </Metric>
            )}
            {hasActuals && (
              <Metric label={tx('actual')} className={misestimate ? 'text-warning' : undefined} title={misestimate ? tx('misestimate') : undefined}>
                <span data-actual="rows">
                  {formatInt(node.actualRows)}
                  {tx('rowsUnit')}
                </span>
                {node.actualLoops !== undefined && <span className="text-muted-foreground"> × {tx('loops', { n: formatInt(node.actualLoops) })}</span>}
                {node.actualTimeMs !== undefined && <span className="text-muted-foreground"> · {formatMs(node.actualTimeMs)}</span>}
              </Metric>
            )}
            {warnings.map((w, i) => (
              <Badge key={`${w.code}-${i}`} tone={BADGE_TONE[w.severity]} title={w.message}>
                {w.code}
              </Badge>
            ))}
          </div>
          {share !== undefined && <CostBar share={share} />}
          <ExtraChips extra={node.extra} />
        </div>
      </div>
      {expanded && (
        <ul role="group">
          {node.children.map((c, i) => {
            const cid = childId(id, i);
            return <PlanNodeItem key={cid} node={c} id={cid} depth={depth + 1} ctx={ctx} />;
          })}
        </ul>
      )}
    </li>
  );
}

export interface PlanTreeProps {
  plan: PlanSummary;
  className?: string;
}

export function PlanTree({ plan, className }: PlanTreeProps) {
  const root = plan.root;
  const matches = useMemo(() => matchWarnings(root, plan.warnings), [root, plan.warnings]);
  const total = useMemo(() => planTotalCost(root, plan.totalCost), [root, plan.totalCost]);
  const expandable = useMemo(() => parentIds(root), [root]);
  const nodeCount = useMemo(() => countNodes(root), [root]);

  // Collapsed ids belong to one plan: a new plan starts fully expanded.
  const [state, setState] = useState<{ plan: PlanSummary; collapsed: ReadonlySet<string> }>(() => ({ plan, collapsed: EMPTY }));
  const collapsed = state.plan === plan ? state.collapsed : EMPTY;
  const update = (fn: (c: ReadonlySet<string>) => ReadonlySet<string>) =>
    setState((s) => ({ plan, collapsed: fn(s.plan === plan ? s.collapsed : EMPTY) }));
  const [showRaw, setShowRaw] = useState(false);

  const treeRef = useRef<HTMLUListElement>(null);
  const focusId = useRef<string | null>(null);
  useEffect(() => {
    const id = focusId.current;
    if (!id) return;
    focusId.current = null;
    const row = treeRef.current?.querySelector<HTMLElement>(`[data-node-id="${id}"] > div`);
    row?.scrollIntoView({ block: 'nearest' });
    row?.focus({ preventScroll: true });
  });

  const ctx: TreeCtx = {
    collapsed,
    total,
    analyzed: plan.analyzed,
    matches,
    toggle: (id) =>
      update((c) => {
        const next = new Set(c);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        return next;
      }),
  };

  const locate = (ids: readonly string[]) => {
    update((c) => {
      const next = new Set(c);
      for (const id of ids) for (const a of ancestorIds(id)) next.delete(a);
      return next;
    });
    focusId.current = ids[0] ?? null;
  };

  return (
    <div className={cn('flex flex-col gap-2', className)}>
      <PlanWarningList warnings={plan.warnings} nodesOf={matches.nodesOf} onLocate={root ? locate : undefined} />
      <div className="flex flex-wrap items-center gap-1">
        {root && (
          <>
            <span className="mr-1 text-xs text-muted-foreground">{tx('nodes', { n: nodeCount })}</span>
            <Button size="sm" variant="ghost" icon={<ChevronsUpDown size={12} />} onClick={() => update(() => EMPTY)} disabled={expandable.length === 0}>
              {tx('expandAll')}
            </Button>
            <Button size="sm" variant="ghost" icon={<ChevronsDownUp size={12} />} onClick={() => update(() => new Set(expandable))} disabled={expandable.length === 0}>
              {tx('collapseAll')}
            </Button>
          </>
        )}
        <Button
          size="sm"
          variant={showRaw ? 'secondary' : 'ghost'}
          icon={<FileCode size={12} />}
          aria-pressed={showRaw}
          onClick={() => setShowRaw((v) => !v)}
          className="ml-auto"
        >
          {t('extract.rawPlan')}
        </Button>
      </div>
      {root && (
        <ul ref={treeRef} role="tree" aria-label={tx('tree')} className="overflow-x-auto rounded border border-border py-1">
          <PlanNodeItem node={root} id={ROOT_ID} depth={0} ctx={ctx} />
        </ul>
      )}
      {showRaw && <RawPlan plan={plan} />}
    </div>
  );
}

/** The original EXPLAIN output, monospace and wrapped. */
export function RawPlan({ plan }: { plan: Pick<PlanSummary, 'raw' | 'rawFormat'> }) {
  return (
    <div className="flex flex-col gap-1">
      {plan.rawFormat && <span className="text-[11px] text-muted-foreground">{tx('rawFormat', { format: plan.rawFormat })}</span>}
      <pre
        aria-label={t('extract.rawPlan')}
        className="max-h-80 overflow-auto whitespace-pre-wrap break-words rounded border border-border bg-muted p-2 font-mono text-[11px] leading-4"
      >
        {plan.raw}
      </pre>
    </div>
  );
}
