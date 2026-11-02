'use client';

// Step 4 계획 (docs/db-copy.md 7). Without a plan: [계획 만들기] (or "계획 계산 중…" with [취소]).
// With one: the facts line of both endpoints, the counts by action, 차단 rows (item blockers offer
// [제외]), 경고 rows from `plan.acks` with "확인했습니다" bound to each key, the notes, the item
// table (28px rows, windowed above 200; existing tables get a 작업 menu that writes
// `existing_overrides` and re-plans; `PLAN_CHANGED` rows highlighted) with a detail split
// (실행할 SQL | 원본 DDL + the item's issues), the session settings of both sides and, for a
// production target, the name confirmation. A warned copy job shows its plan read-only here, and
// so does a job rejected by its start-time plan (its blockers are the reason). MySQL
// column findings (`BINARY_ATTR_UNDECODABLE`, `BLOB_HOLDS_TEXT`) offer "이 열을 바이너리 / 텍스트로
// 처리", which writes `mysql.columns` and re-plans.

import type { AckNeed, ColumnTreatment, CopyIssue, CopyPlan, PlanItem } from 'ipc-types';
import { ArrowRight, ChevronDown } from 'lucide-react';
import { Fragment, useMemo, useRef, useState, type UIEvent } from 'react';
import { Button, CodeBlock, CopyButton, DropdownMenu, EnvTag, Input, Notice, SegmentedControl, Spinner, useListNavigation, type MenuItem } from '@/components/ui';
import { connTag } from '@/lib/env';
import { formatBytes, formatCompact, formatDuration } from '@/lib/format';
import { cn } from '@/lib/utils';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import {
  actionPolicy,
  allIssues,
  ackIssue,
  COLUMN_TREATMENT,
  driverLabel,
  existingCounts,
  factsEndpoint,
  factsParts,
  hasPolicyMenu,
  infoIssues,
  isNdb,
  issueColumn,
  itemIssues,
  planChangedIds,
  POLICIES,
  sortedItems,
  targetStateKey,
} from './model';
import { ActionChip, Caption, Disclosure, IssueRow, KindIcon } from './parts';
import { cs, k } from './strings';

const ROW = 28;
const WINDOW_ABOVE = 200;
const LIST_HEIGHT = 336;
const COLLATION_CODES = new Set(['ENUM_DUPLICATE', 'COLLATION_UNIQUE_RISK', 'UNIQUE_PREFIX']);

/** Collations offered for the plan's effective character set (a `_bin` collation exists for every one). */
function collationChoicesFor(charset: string | undefined): string[] {
  return (charset ?? 'utf8mb4') === 'utf8mb4' ? ['utf8mb4_bin', 'utf8mb4_unicode_520_ci'] : [`${charset}_bin`];
}

/** "이 테이블만 정렬 규칙 바꾸기…": one entry per affected table and collation (MySQL). */
function CollationMenu({ plan, itemIds }: { plan: CopyPlan; itemIds: string[] }) {
  const tables = plan.items.filter((i) => i.kind === 'table' && itemIds.includes(i.id));
  if (plan.family !== 'mysql' || tables.length === 0) return null;
  const choices = collationChoicesFor(plan.effective.charset);
  const items: MenuItem[] = tables.flatMap((t) =>
    choices.map((c) => ({ id: `${t.id}:${c}`, label: cs('collationFor', { table: t.name, collation: c }), onSelect: () => void useCopyStore.getState().setTableCollation(t.name, c) })),
  );
  return (
    <DropdownMenu items={items} align="start">
      <Button size="sm" variant="ghost">
        {cs('changeCollation')}
      </Button>
    </DropdownMenu>
  );
}

/** The (source table, column) a column finding names, when both are known (MySQL plans only). */
function columnTarget(plan: CopyPlan, issue: CopyIssue): { table: string; column: string; treatAs: ColumnTreatment } | null {
  const treatAs = COLUMN_TREATMENT[issue.code];
  if (!treatAs || plan.family !== 'mysql' || !issue.itemId) return null;
  const table = plan.items.find((i) => i.id === issue.itemId && i.kind === 'table');
  const column = issueColumn(issue);
  return table && column ? { table: table.name, column, treatAs } : null;
}

type ColumnTarget = NonNullable<ReturnType<typeof columnTarget>>;

/** The distinct columns a set of issues can be fixed for through `mysql.columns`. */
function columnTargets(plan: CopyPlan, issues: CopyIssue[]): ColumnTarget[] {
  const all = issues.map((i) => columnTarget(plan, i)).filter((t): t is ColumnTarget => !!t);
  return all.filter((t, i) => all.findIndex((u) => u.table === t.table && u.column === t.column) === i);
}

/**
 * "이 열을 바이너리로 처리" / "이 열을 텍스트로 처리" for column findings: one button for one
 * column, a menu of `table.column` entries for several (the grouped notes row).
 */
function ColumnTreatAction({ targets: unique }: { targets: ColumnTarget[] }) {
  if (unique.length === 0) return null;
  const apply = (t: ColumnTarget) => void useCopyStore.getState().setColumnTreatment(t.table, t.column, t.treatAs);
  if (unique.length === 1) {
    const t = unique[0];
    return (
      <Button size="sm" variant="secondary" title={cs('treatAsFor', { table: t.table, column: t.column })} onClick={() => apply(t)}>
        {cs(k(`treatAs.${t.treatAs}`))}
      </Button>
    );
  }
  const items: MenuItem[] = unique.map((t) => ({ id: `${t.table}.${t.column}`, label: cs('treatAsFor', { table: t.table, column: t.column }), onSelect: () => apply(t) }));
  return (
    <DropdownMenu items={items} align="start">
      <Button size="sm" variant="ghost">
        {cs(k(`treatAsMenu.${unique[0].treatAs}`))}
      </Button>
    </DropdownMenu>
  );
}

function Facts({ plan }: { plan: CopyPlan }) {
  const conns = useConnectionsStore((s) => s.connections);
  const draft = useCopyStore((s) => s.wizard.draft);
  const src = conns.find((c) => c.id === draft.source.connId);
  const tgt = conns.find((c) => c.id === draft.target.connId);
  const side = (label: string, name: string | undefined, f: CopyPlan['source'], tag: ReturnType<typeof connTag>) => (
    <div className="flex min-w-0 items-center gap-2">
      <span className="w-10 shrink-0 text-fg-secondary">{label}</span>
      {tag && <EnvTag tag={tag} variant="chip" />}
      <span className="min-w-0 truncate" data-selectable title={factsEndpoint(f)}>
        {name ? `${name} · ` : ''}
        {driverLabel(f.driver)} {f.version} · {factsEndpoint(f)}
        {f.charset ? ` · ${f.charset}` : ''}
      </span>
    </div>
  );
  return (
    <section className="flex flex-col gap-1 text-base" aria-label={cs('factsAria')}>
      {side(cs('factsSource'), src?.name, plan.source, connTag(src))}
      {side(cs('factsTarget'), tgt?.name, plan.target, connTag(tgt))}
      <div className="text-sm tabular-nums text-fg-secondary" data-testid="plan-facts">
        {/* Lines break between the facts, never inside one ("대상 세션 3"). */}
        {factsParts(plan, (n) => formatBytes(n, 0)).map((p, i) => (
          <Fragment key={i}>
            {i > 0 && ' · '}
            <span className="whitespace-nowrap">{p}</span>
          </Fragment>
        ))}
      </div>
      {plan.family === 'mysql' && <div className="text-sm text-fg-tertiary">{cs('mysqlVersionsCaption')}</div>}
      {plan.family === 'mysql' && [plan.source.version, plan.target.version].filter(isNdb).map((v) => <Notice key={v} tone="info">{cs('ndbNotice', { version: v })}</Notice>)}
    </section>
  );
}

function Counts({ plan }: { plan: CopyPlan }) {
  const t = plan.totals;
  const ex = existingCounts(plan);
  const parts = (
    [
      ['create', t.create, false],
      ['skip', t.skip, false],
      ['truncate', t.truncate, true],
      ['drop_recreate', t.dropRecreate, true],
      ['append', t.append, false],
      ['unsupported', t.unsupported, false],
      ['excluded', t.excluded, false],
    ] as const
  )
    .filter(([key, n]) => n > 0 || key === 'create' || key === 'skip')
    .map(([key, n, danger]) => ({ key, text: cs(k(`count.${key}`), { n }), danger }));
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm tabular-nums" data-testid="plan-counts">
      {parts.map((p) => (
        <span key={p.key} className={p.danger ? 'font-medium text-danger' : 'text-fg-secondary'}>
          {p.text}
        </span>
      ))}
      {ex.existing > 0 && <span className="text-fg-secondary">{cs('count.existing', { n: ex.existing, m: ex.existingEmpty })}</span>}
      <span className="text-fg-secondary">{cs('count.rows', { rows: formatCompact(t.estRows), size: formatBytes(t.estBytes) })}</span>
    </div>
  );
}

function PolicyMenu({ item, disabled }: { item: PlanItem; disabled: boolean }) {
  const overrides = useCopyStore((s) => s.wizard.draft.options.existingOverrides);
  const global = useCopyStore((s) => s.wizard.draft.options.existing);
  const current = actionPolicy(item.action);
  const overridden = overrides.some((o) => o.itemId === item.id);
  const items: MenuItem[] = [
    ...POLICIES.map((p) => ({
      id: p,
      kind: 'radio' as const,
      checked: current === p,
      danger: p === 'truncate' || p === 'drop_recreate',
      label: cs(k(`existing.${p}`)),
      onSelect: () => void useCopyStore.getState().setItemPolicy(item.id, p),
    })),
    ...(overridden
      ? [{ id: 'default', separatorBefore: true, label: cs('useDefaultPolicy', { policy: cs(k(`existing.${global}`)) }), onSelect: () => void useCopyStore.getState().setItemPolicy(item.id, null) }]
      : []),
  ];
  if (disabled) return <ActionChip action={item.action} />;
  return (
    <DropdownMenu items={items} align="start">
      <button
        type="button"
        aria-label={cs('actionMenu', { name: item.targetName })}
        className="inline-flex items-center gap-0.5 rounded-sm focus-visible:outline-2 focus-visible:outline-focus"
        onClick={(e) => e.stopPropagation()}
      >
        <ActionChip action={item.action} />
        <ChevronDown size={12} strokeWidth={1.75} className="text-fg-secondary" />
      </button>
    </DropdownMenu>
  );
}

function ItemDetail({ plan, item, readOnly }: { plan: CopyPlan; item: PlanItem; readOnly: boolean }) {
  const [tab, setTab] = useState<'sql' | 'ddl'>('sql');
  const sql = item.targetSql.map((s) => `${s};`).join('\n\n');
  const issues = itemIssues(plan, item.id);
  return (
    <section className="flex flex-col gap-2 border-t border-separator pt-2" aria-label={cs('detailAria')}>
      <div className="flex items-center gap-2">
        <KindIcon kind={item.kind} />
        <span className="min-w-0 truncate font-medium">{item.targetSchema ? `${item.targetSchema}.${item.targetName}` : item.targetName}</span>
        <SegmentedControl
          variant="tabs"
          aria-label={cs('detailAria')}
          value={tab}
          onChange={setTab}
          options={[
            { value: 'sql', label: cs('detailSql') },
            { value: 'ddl', label: cs('detailDdl') },
          ]}
          className="ml-auto"
        />
        <CopyButton size="sm" label={tab === 'sql' ? cs('copySql') : cs('copyDdl')} getText={() => (tab === 'sql' ? sql : (item.sourceDdl ?? ''))} />
      </div>
      {tab === 'sql' ? (
        sql ? <CodeBlock code={sql} maxHeight={180} aria-label={cs('detailSql')} /> : <div className="text-sm text-fg-secondary">{cs('noSql')}</div>
      ) : item.sourceDdl ? (
        <CodeBlock code={item.sourceDdl} maxHeight={180} aria-label={cs('detailDdl')} />
      ) : (
        <div className="text-sm text-fg-secondary">{cs('noDdl')}</div>
      )}
      {issues.length > 0 && (
        <ul className="flex flex-col">
          {issues.map((i, n) => {
            const targets = readOnly ? [] : columnTargets(plan, [{ ...i, itemId: i.itemId ?? item.id }]);
            return (
              <IssueRow key={`${i.code}-${n}`} issue={i}>
                {targets.length > 0 && <ColumnTreatAction targets={targets} />}
              </IssueRow>
            );
          })}
        </ul>
      )}
    </section>
  );
}

function ItemTable({ plan, readOnly }: { plan: CopyPlan; readOnly: boolean }) {
  const items = useMemo(() => sortedItems(plan), [plan]);
  const changed = useMemo(() => planChangedIds(plan), [plan]);
  const issueCount = useMemo(() => {
    const m = new Map<string, number>();
    for (const it of plan.items) m.set(it.id, it.warnings.length);
    for (const b of [...plan.blockers, ...plan.warnings]) if (b.itemId) m.set(b.itemId, (m.get(b.itemId) ?? 0) + 1);
    return m;
  }, [plan]);
  const [active, setActive] = useState(-1);
  const [scrollTop, setScrollTop] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const nav = useListNavigation({ count: items.length, activeIndex: active, onMove: setActive, onOpen: (i) => setActive(i) });
  const windowed = items.length > WINDOW_ABOVE;
  const first = windowed ? Math.max(0, Math.floor(scrollTop / ROW) - 10) : 0;
  const last = windowed ? Math.min(items.length, first + Math.ceil(LIST_HEIGHT / ROW) + 20) : items.length;
  const selected = active >= 0 ? items[active] : undefined;
  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    if (windowed) setScrollTop(e.currentTarget.scrollTop);
  };
  const th = 'px-2 text-left text-sm font-normal text-fg-secondary';
  return (
    <section className="flex flex-col gap-1">
      <Caption>
        {cs('items')} <span className="font-normal tabular-nums text-fg-tertiary">{items.length}</span>
      </Caption>
      <div
        ref={listRef}
        {...nav.listProps}
        aria-label={cs('itemsAria')}
        onScroll={onScroll}
        className="relative scroll-pt-6 overflow-auto outline-none focus-visible:outline-2 focus-visible:outline-focus"
        style={{ maxHeight: LIST_HEIGHT }}
      >
        {/* Inside the scroller, so the columns line up with the rows whether or not a scrollbar shows. */}
        <div className="sticky top-0 z-[1] flex h-6 items-center bg-panel" aria-hidden>
          <span className={cn(th, 'w-12 shrink-0 text-right')}>{cs('col.order')}</span>
          <span className={cn(th, 'min-w-0 flex-1')}>{cs('col.object')}</span>
          <span className={cn(th, 'w-36 shrink-0')}>{cs('col.action')}</span>
          <span className={cn(th, 'w-32 shrink-0')}>{cs('col.target')}</span>
          <span className={cn(th, 'w-20 shrink-0 text-right')}>{cs('col.rows')}</span>
          <span className={cn(th, 'w-12 shrink-0 text-right')}>{cs('col.issues')}</span>
        </div>
        <div style={windowed ? { height: items.length * ROW, position: 'relative' } : undefined}>
          {items.slice(first, last).map((it, n) => {
            const i = first + n;
            const stateKey = targetStateKey(it);
            const isChanged = changed.has(it.id);
            const count = issueCount.get(it.id) ?? 0;
            return (
              <div
                key={it.id}
                {...nav.getItemProps(i)}
                data-item={it.id}
                data-changed={isChanged || undefined}
                className={cn(
                  'flex h-7 items-center border-t border-separator text-base first:border-t-0',
                  i === active ? 'bg-selected' : 'hover:bg-hover',
                  isChanged && i !== active && 'bg-warning-soft',
                )}
                style={windowed ? { position: 'absolute', top: i * ROW, left: 0, right: 0 } : undefined}
              >
                <span className="w-12 shrink-0 px-2 text-right text-sm tabular-nums text-fg-tertiary" title={cs(k(`phase.${it.phase}`))}>
                  {i + 1}
                </span>
                <span className="flex min-w-0 flex-1 items-center gap-1.5 px-2">
                  <KindIcon kind={it.kind} />
                  <span className="min-w-0 truncate" title={it.id}>
                    {it.schema ? `${it.schema}.` : ''}
                    {it.name}
                  </span>
                  {(it.targetName !== it.name || (it.targetSchema ?? it.schema) !== it.schema) && (
                    <>
                      <ArrowRight size={12} className="shrink-0 text-fg-tertiary" />
                      <span className="min-w-0 truncate text-fg-secondary">
                        {it.targetSchema ? `${it.targetSchema}.` : ''}
                        {it.targetName}
                      </span>
                    </>
                  )}
                  {isChanged && <span className="sr-only">{cs('changed')}</span>}
                </span>
                <span className="w-36 shrink-0 px-2">{hasPolicyMenu(it) ? <PolicyMenu item={it} disabled={readOnly} /> : <ActionChip action={it.action} />}</span>
                <span className="w-32 shrink-0 truncate px-2 text-sm text-fg-secondary">{stateKey ? cs(stateKey) : it.kind === 'table' ? cs('exists.no') : ''}</span>
                <span className="w-20 shrink-0 px-2 text-right text-sm tabular-nums text-fg-secondary">{it.estRows !== undefined && it.hasData ? `≈${formatCompact(it.estRows)}` : ''}</span>
                <span className={cn('w-12 shrink-0 px-2 text-right text-sm tabular-nums', count > 0 ? 'text-warning' : 'text-fg-tertiary')}>{count > 0 ? count : ''}</span>
              </div>
            );
          })}
        </div>
      </div>
      {selected && <ItemDetail key={selected.id} plan={plan} item={selected} readOnly={readOnly} />}
    </section>
  );
}

function AckRow({ plan, ack, readOnly }: { plan: CopyPlan; ack: AckNeed; readOnly: boolean }) {
  const checked = useCopyStore((s) => s.wizard.acks.includes(ack.key));
  const issue: CopyIssue = ackIssue(plan, ack) ?? { code: ack.code, severity: ack.severity, message: ack.code };
  // With several items the first issue's message names only one of them: list them instead.
  const names = ack.items.map((id) => {
    const it = plan.items.find((i) => i.id === id);
    return it ? `${it.targetSchema ? `${it.targetSchema}.` : ''}${it.targetName}` : id;
  });
  const affected = names.length > 1 ? `${names.slice(0, 6).join(', ')}${names.length > 6 ? ` +${names.length - 6}` : ''}` : undefined;
  // A detail is shown only when every issue of this code says the same (else it names one item).
  const details = new Set(allIssues(plan).filter((i) => i.code === ack.code).map((i) => i.detail ?? ''));
  const detail = details.size === 1 ? issue.detail : undefined;
  return (
    <IssueRow
      issue={{ ...issue, severity: ack.severity, detail }}
      count={ack.items.length}
      affected={affected}
      ack={{ checked, onChange: () => useCopyStore.getState().toggleAck(ack.key) }}
    >
      {!readOnly && COLLATION_CODES.has(ack.code) && <CollationMenu plan={plan} itemIds={ack.items} />}
    </IssueRow>
  );
}

function ProductionConfirm({ plan }: { plan: CopyPlan }) {
  const targetId = useCopyStore((s) => s.wizard.draft.target.connId);
  const target = useConnectionsStore((s) => s.connections.find((c) => c.id === targetId));
  const name = useCopyStore((s) => s.wizard.confirmName);
  if (target?.tag !== 'production') return null;
  return (
    <Notice tone="danger" title={cs('productionTitle', { endpoint: factsEndpoint(plan.target) })}>
      <label className="mt-1 flex flex-col gap-1">
        <span>{cs('productionPrompt', { name: target.name })}</span>
        <Input aria-label={cs('productionInput')} value={name} onChange={(e) => useCopyStore.getState().setConfirmName(e.target.value)} className="w-80" autoComplete="off" spellCheck={false} />
      </label>
    </Notice>
  );
}

export function PlanStep() {
  const w = useCopyStore((s) => s.wizard);
  const status = useCopyStore((s) => (s.wizard.jobId ? s.runs[s.wizard.jobId] : undefined));
  const [notesOpen, setNotesOpen] = useState(false);
  const plan = w.plan ?? status?.plan ?? null;
  const jobMode = !!w.jobId;
  const warned = status?.state === 'warned';
  const rejected = jobMode && status?.state === 'rejected' && !status.result;

  if (!plan) {
    return (
      <div className="flex flex-col items-start gap-3">
        {w.error && (
          <Notice tone="danger" className="self-stretch">
            {w.error}
          </Notice>
        )}
        {w.planBusy ? (
          <div className="flex items-center gap-2 text-fg-secondary" role="status">
            <Spinner size={14} decorative />
            {cs('planning')}
            <Button size="sm" variant="secondary" onClick={() => void useCopyStore.getState().cancelPlan()}>
              {cs('cancelPlan')}
            </Button>
          </div>
        ) : (
          <div className="text-fg-secondary">{cs('planIntro')}</div>
        )}
      </div>
    );
  }

  const readOnly = jobMode;
  const notes = infoIssues(plan);
  const changed = planChangedIds(plan);
  return (
    <div className="flex flex-col gap-4">
      {w.error && <Notice tone="danger">{w.error}</Notice>}
      {warned && <Notice tone="warning">{cs('warnedJob')}</Notice>}
      {/* The blockers below are the job's reason (core rejects with exactly these). */}
      {rejected && status && (
        <Notice tone="danger" title={cs('ended.rejected')}>
          <span data-testid="job-end" data-state="rejected">
            {status.runId !== status.jobId ? cs('rejectedResume') : cs('rejectedJob')}
          </span>
        </Notice>
      )}
      {changed.size > 0 && <Notice tone="warning">{cs('planChangedNotice')}</Notice>}
      <Facts plan={plan} />
      <Counts plan={plan} />

      {plan.blockers.length > 0 && (
        <section className="flex flex-col">
          <Caption tone="danger">{cs('blockers')}</Caption>
          <ul className="flex flex-col" aria-label={cs('blockers')}>
            {plan.blockers.map((b, i) => {
              const targets = readOnly ? [] : columnTargets(plan, [b]);
              return (
                <IssueRow key={`${b.code}-${i}`} issue={b}>
                  {targets.length > 0 && <ColumnTreatAction targets={targets} />}
                  {!readOnly && b.itemId && (
                    <Button size="sm" variant="secondary" onClick={() => void useCopyStore.getState().exclude(b.itemId!)}>
                      {cs('exclude')}
                    </Button>
                  )}
                  {!readOnly && b.itemId && COLLATION_CODES.has(b.code) && <CollationMenu plan={plan} itemIds={[b.itemId]} />}
                </IssueRow>
              );
            })}
          </ul>
        </section>
      )}
      {plan.acks.length > 0 && (
        <section className="flex flex-col">
          <Caption tone="warning">{cs('warnings')}</Caption>
          <ul className="flex flex-col" aria-label={cs('warnings')}>
            {plan.acks.map((a) => (
              <AckRow key={a.key} plan={plan} ack={a} readOnly={readOnly} />
            ))}
          </ul>
        </section>
      )}
      {plan.blockers.length === 0 && plan.acks.length === 0 && <div className="text-fg-secondary">{cs('noIssues')}</div>}
      {notes.length > 0 && (
        <Disclosure open={notesOpen} onToggle={() => setNotesOpen((v) => !v)} title={cs('notes', { n: notes.length })}>
          <ul className="flex flex-col">
            {notes.map(({ issue, count }) => {
              const targets = readOnly || !COLUMN_TREATMENT[issue.code] ? [] : columnTargets(plan, allIssues(plan).filter((x) => x.code === issue.code));
              return (
                <IssueRow key={issue.code} issue={issue} count={count > 1 ? count : undefined}>
                  {targets.length > 0 && <ColumnTreatAction targets={targets} />}
                </IssueRow>
              );
            })}
          </ul>
        </Disclosure>
      )}

      <ItemTable plan={plan} readOnly={readOnly} />

      <section className="flex flex-col gap-1.5">
        <Caption>{cs('sessionSettings')}</Caption>
        <div className="grid grid-cols-2 gap-3">
          {(['source', 'target'] as const).map((side) => (
            <div key={side} className="flex min-w-0 flex-col gap-1">
              <span className="text-sm text-fg-secondary">{side === 'source' ? cs('sessionSource') : cs('sessionTarget')}</span>
              {plan.sessionSql[side].length > 0 ? (
                <CodeBlock code={plan.sessionSql[side].map((s) => `${s};`).join('\n')} maxHeight={120} aria-label={`${cs('sessionSettings')} ${side === 'source' ? cs('sessionSource') : cs('sessionTarget')}`} />
              ) : (
                <span className="text-sm text-fg-tertiary">{cs('none')}</span>
              )}
            </div>
          ))}
        </div>
      </section>

      {!readOnly && <ProductionConfirm plan={plan} />}
      <div className="flex flex-wrap gap-x-3 text-sm text-fg-tertiary">
        <span>{cs('planAt', { t: formatDuration(plan.elapsedMs) })}</span>
        <span className="font-mono text-xs" data-selectable>
          {plan.planId}
        </span>
        {!readOnly && <span>{cs('dryRunNote')}</span>}
      </div>
    </div>
  );
}
