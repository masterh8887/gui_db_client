'use client';

// Small presentational pieces of the copy wizard (DESIGN.md §3 tokens, §7.4 primitives): section
// captions, code chips, the plan action / item state chips, kind icons, issue rows and a native
// radio list. No state of their own.

import type { CopyIssue, CopyItemState, CopyObjectKind, PlanAction } from 'ipc-types';
import {
  ArrowDownAZ,
  ChartColumn,
  ChevronDown,
  ChevronRight,
  CircleX,
  Database,
  Eraser,
  Eye,
  Folder,
  Hash,
  Info,
  KeyRound,
  Layers,
  Link2,
  ListOrdered,
  Lock,
  MessageSquare,
  Puzzle,
  RefreshCw,
  Shapes,
  ShieldCheck,
  SquareFunction,
  Table2,
  TriangleAlert,
  Users,
  Zap,
  type LucideIcon,
} from 'lucide-react';
import { useId, type ReactNode } from 'react';
import { Checkbox, Spinner } from '@/components/ui';
import { cn } from '@/lib/utils';
import { issueTitle, recommendationText } from './model';
import { cs, k } from './strings';

export function Caption({ children, tone, className }: { children: ReactNode; tone?: 'danger' | 'warning'; className?: string }) {
  return (
    <h3 className={cn('text-sm font-semibold text-fg-secondary', tone === 'danger' && 'text-danger', tone === 'warning' && 'text-warning', className)}>{children}</h3>
  );
}

export function CodeChip({ children }: { children: ReactNode }) {
  return <code className="inline-flex h-[18px] items-center rounded-sm bg-control px-1.5 font-mono text-xs text-fg-secondary">{children}</code>;
}

const KIND_ICON: Record<CopyObjectKind, LucideIcon> = {
  database: Database,
  schema: Folder,
  extension: Puzzle,
  collation: ArrowDownAZ,
  type: Shapes,
  domain: Shapes,
  sequence: Hash,
  function: SquareFunction,
  table: Table2,
  partition: Table2,
  view: Eye,
  materialized_view: Layers,
  index: ListOrdered,
  constraint: KeyRound,
  foreign_key: Link2,
  trigger: Zap,
  policy: ShieldCheck,
  comment: MessageSquare,
  owner: Users,
  grant: Lock,
  truncate_group: Eraser,
  sequence_value: Hash,
  auto_increment: Hash,
  refresh: RefreshCw,
  analyze: ChartColumn,
};

export function KindIcon({ kind }: { kind: CopyObjectKind }) {
  const Icon = KIND_ICON[kind] ?? Table2;
  return <Icon size={14} strokeWidth={1.75} className="shrink-0 text-fg-secondary" aria-label={cs(k(`kind.${kind}`))} role="img" />;
}

const ACTION_TONE: Record<PlanAction, string> = {
  create: 'bg-accent-soft text-accent',
  skip_existing: 'bg-control text-fg-secondary',
  truncate: 'bg-danger-soft text-danger',
  drop_recreate: 'bg-danger-soft text-danger',
  append: 'bg-warning-soft text-warning',
  run: 'bg-control text-fg-secondary',
  unsupported: 'bg-warning-soft text-warning',
  excluded: 'bg-transparent text-fg-tertiary shadow-[inset_0_0_0_1px_var(--separator-strong)]',
  resume_done: 'bg-success-soft text-success',
};

/** The 작업 chip of a plan row (destructive actions in danger text). */
export function ActionChip({ action, className }: { action: PlanAction; className?: string }) {
  return (
    <span data-action={action} className={cn('inline-flex h-[18px] shrink-0 items-center whitespace-nowrap rounded-sm px-1.5 text-sm font-medium leading-none', ACTION_TONE[action], className)}>
      {cs(k(`action.${action}`))}
    </span>
  );
}

const STATE_TONE: Record<CopyItemState, string> = {
  pending: 'bg-control text-fg-secondary',
  creating: 'bg-accent-soft text-accent',
  loading: 'bg-accent-soft text-accent',
  indexing: 'bg-accent-soft text-accent',
  verifying: 'bg-accent-soft text-accent',
  done: 'bg-success-soft text-success',
  skipped: 'bg-control text-fg-secondary',
  failed: 'bg-danger-soft text-danger',
  cancelled: 'bg-warning-soft text-warning',
  planned: 'bg-control text-fg-secondary',
};

const BUSY: readonly CopyItemState[] = ['creating', 'loading', 'indexing', 'verifying'];

/** State chip of a table row in the progress view (text + spinner/icon, never colour alone). */
export function ItemStateChip({ state }: { state: CopyItemState }) {
  return (
    <span data-state={state} className={cn('inline-flex h-[18px] shrink-0 items-center gap-1 whitespace-nowrap rounded-sm px-1.5 text-sm font-medium leading-none', STATE_TONE[state])}>
      {BUSY.includes(state) && <Spinner size={10} tone="current" decorative />}
      {state === 'failed' && <CircleX size={11} strokeWidth={2} />}
      {cs(k(`state.${state}`))}
    </span>
  );
}

/** One plan issue: icon, title (known code) or message, code chip, detail, recommendation, extras. */
export function IssueRow({
  issue,
  count,
  affected,
  ack,
  children,
}: {
  issue: CopyIssue;
  count?: number;
  /** The affected items, listed instead of the first item's own message. */
  affected?: string;
  ack?: { checked: boolean; onChange: (v: boolean) => void };
  children?: ReactNode;
}) {
  const id = useId();
  const Icon = issue.severity === 'danger' ? CircleX : issue.severity === 'warning' ? TriangleAlert : Info;
  const tone = issue.severity === 'danger' ? 'text-danger' : issue.severity === 'warning' ? 'text-warning' : 'text-accent';
  const title = issueTitle(issue);
  const recommendation = recommendationText(issue);
  return (
    <li className="flex items-start gap-2 border-t border-separator py-2 first:border-t-0" data-code={issue.code}>
      <Icon size={14} strokeWidth={1.75} className={cn('mt-0.5 shrink-0', tone)} />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className={cn('font-medium', issue.severity === 'danger' && 'text-danger')}>{title}</span>
          <CodeChip>{issue.code}</CodeChip>
          {count !== undefined && count > 0 && <span className="text-sm text-fg-secondary">{cs('affected', { n: count })}</span>}
        </div>
        {affected ? (
          <div className="text-sm text-fg-secondary" data-selectable>
            {affected}
          </div>
        ) : (
          title !== issue.message && (
            <div className="text-sm text-fg-secondary" data-selectable>
              {issue.message}
            </div>
          )
        )}
        {issue.detail && (
          <div className="text-sm text-fg-secondary" data-selectable>
            {issue.detail}
          </div>
        )}
        {recommendation && (
          <div className="text-sm">
            <span className="text-fg-secondary">{cs('recommendation')}:</span> {recommendation}
          </div>
        )}
        {(ack || children) && (
          <div className="mt-1.5 flex flex-wrap items-center gap-3">
            {ack && (
              <label htmlFor={id} className="flex w-fit items-center gap-2 text-base">
                <Checkbox id={id} checked={ack.checked} onChange={ack.onChange} aria-label={cs('acknowledgeAria', { code: issue.code })} />
                {cs('acknowledge')}
              </label>
            )}
            {children}
          </div>
        )}
      </div>
    </li>
  );
}

/**
 * A list of native radios with optional help lines (keyboard: arrows, Space). `inline` lays them
 * out in one row (short choices, e.g. 전체 / 테이블 선택). Every choice in the wizard except the
 * steps bar is one of these (DESIGN.md "Copy wizard look"), so none looks like the steps'
 * SegmentedControl.
 */
export function RadioList<V extends string>({
  name,
  value,
  onChange,
  options,
  label,
  disabled,
  inline,
}: {
  name: string;
  value: V;
  onChange: (v: V) => void;
  options: { value: V; label: ReactNode; help?: ReactNode; danger?: boolean; disabled?: boolean }[];
  label: string;
  disabled?: boolean;
  inline?: boolean;
}) {
  const uid = useId();
  return (
    <div role="radiogroup" aria-label={label} className={inline ? 'flex flex-wrap items-start gap-x-5 gap-y-1' : 'flex flex-col gap-1'}>
      {options.map((o) => {
        const id = `${uid}-${o.value}`;
        const off = disabled || o.disabled;
        return (
          <label key={o.value} htmlFor={id} className={cn('flex items-start gap-2', off && 'opacity-45')}>
            <input
              id={id}
              type="radio"
              name={`${uid}-${name}`}
              value={o.value}
              checked={value === o.value}
              disabled={off}
              onChange={() => onChange(o.value)}
              aria-labelledby={`${id}-l`}
              aria-describedby={o.help ? `${id}-h` : undefined}
              className="mt-[3px] h-3.5 w-3.5 shrink-0 accent-[var(--accent-fill)] focus-visible:outline-2 focus-visible:outline-focus"
            />
            <span className="min-w-0">
              <span id={`${id}-l`} className={cn('text-base', o.danger && 'text-danger')}>
                {o.label}
              </span>
              {o.help && (
                <span id={`${id}-h`} className="block text-sm text-fg-secondary">
                  {o.help}
                </span>
              )}
            </span>
          </label>
        );
      })}
    </div>
  );
}

/** A collapsed-by-default section header (고급, 참고). */
export function Disclosure({ open, onToggle, title, children }: { open: boolean; onToggle: () => void; title: ReactNode; children: ReactNode }) {
  const id = useId();
  const Icon = open ? ChevronDown : ChevronRight;
  return (
    <section className="flex flex-col gap-2">
      <button type="button" aria-expanded={open} aria-controls={id} onClick={onToggle} className="flex w-fit items-center gap-1 rounded-md text-sm font-semibold text-fg-secondary focus-visible:outline-2 focus-visible:outline-focus">
        <Icon size={14} strokeWidth={1.75} />
        {title}
      </button>
      {open && (
        <div id={id} className="flex flex-col gap-3 pl-[18px]">
          {children}
        </div>
      )}
    </section>
  );
}
