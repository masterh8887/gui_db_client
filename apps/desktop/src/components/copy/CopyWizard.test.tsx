// The copy wizard against the browser mock (docs/db-copy.md 7): step gating, the target picker
// (same family, write-allowed only, the mark-as-writable flow), the same-database refusal, the
// production notice and confirmation, objects and options (skip is the visible default), the plan
// (acknowledgements, [제외], the per-table action menu, the SQL split), keyboard flow, progress
// with a confirmed stop, the result with what stays behind and resume, dry runs, a warned job and
// the Activity slot.

import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { Connection, CopyPlan, CopyRequest, JobSummary } from 'ipc-types';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { TooltipProvider } from '@/components/ui';
import { connSave, copyPlan, copyStart } from '@/lib/commands';
import { setConfirmHandler, type ConfirmOptions } from '@/lib/confirm';
import { subscribeAll } from '@/lib/events';
import { LEGACY_CONN_ID, mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { useCopyStore, type CopyPreset } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { resetStores } from '@/test/stores';
import { CopyJobSummary } from './CopyJobSummary';
import { CopyWizard } from './CopyWizard';
import { infoIssues } from './model';
import { cs, k } from './strings';

const store = () => useCopyStore.getState();
const wiz = () => useCopyStore.getState().wizard;
const dialog = () => screen.getByRole('dialog');
const button = (name: string) => within(dialog()).getByRole('button', { name });
const stepRadio = (step: string) => within(dialog()).getByRole('radio', { name: cs(k(`step.${step}`)) });
/** Radio groups drawn as a SegmentedControl (button radios), not as native radio lists. */
const segmented = () => within(dialog()).getAllByRole('radiogroup').filter((g) => within(g).queryAllByRole('radio').some((r) => r.tagName === 'BUTTON'));

async function allowWrite(connId: string): Promise<void> {
  const conn = useConnectionsStore.getState().connections.find((c) => c.id === connId)!;
  await connSave({ ...conn, writeAllowed: true });
  await useConnectionsStore.getState().load();
}

async function mysql8(): Promise<Connection> {
  const created = await connSave({ ...mockLegacyConnection(), id: '', name: 'mysql8', host: 'mysql8.internal', fields: { sslmode: 'prefer' } });
  const saved = await connSave({ ...created, writeAllowed: true });
  await useConnectionsStore.getState().load();
  return saved;
}

function renderWizard() {
  return render(
    <TooltipProvider>
      <CopyWizard />
    </TooltipProvider>,
  );
}

function open(preset: CopyPreset) {
  act(() => store().open(preset));
}

/** Plan from the footer and wait for the plan to show. */
async function makePlan(user: ReturnType<typeof userEvent.setup>) {
  if (wiz().step !== 'plan') await user.click(stepRadio('plan'));
  await user.click(button(cs('makePlan')));
  await waitFor(() => expect(wiz().plan).not.toBeNull());
}

async function ackAll(user: ReturnType<typeof userEvent.setup>) {
  for (const a of wiz().plan!.acks) await user.click(within(dialog()).getByRole('checkbox', { name: cs('acknowledgeAria', { code: a.code }) }));
}

describe('CopyWizard', () => {
  beforeEach(async () => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0, progressTickMs: 4 });
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()], drivers: mockDrivers(), loaded: true });
    // What AppEvents does for the jobs store (the wizard follows job states through it).
    await subscribeAll({
      onJobState: (ev) => {
        const r = useJobsStore.getState().applyState(ev);
        if (!r.known || r.refetch) void useJobsStore.getState().refreshJob(ev.jobId);
      },
      onJobProgress: (p) => useJobsStore.getState().applyProgress(p),
      onJobDone: (d) => useJobsStore.getState().applyDone(d),
    });
  });
  afterEach(() => setConfirmHandler(null));

  it('renders nothing while closed; steps 2-4 open only once both endpoints are valid', async () => {
    const user = userEvent.setup();
    renderWizard();
    expect(screen.queryByRole('dialog')).toBeNull();
    open({ source: { connId: 'c-prod' } });
    expect(await screen.findByRole('dialog')).toHaveTextContent(cs('title'));
    expect(stepRadio('endpoints')).toHaveAttribute('aria-checked', 'true');
    for (const s of ['objects', 'options', 'plan']) expect(stepRadio(s)).toHaveAttribute('aria-disabled', 'true');
    expect(button(cs('next'))).toBeDisabled();
    await allowWrite('c-staging');
    act(() => store().setDraft({ target: { connId: 'c-staging', database: 'analytics_dw' } }));
    await waitFor(() => expect(button(cs('next'))).toBeEnabled());
    expect(stepRadio('plan')).not.toHaveAttribute('aria-disabled');
    // Defaults and the whole database: the plan can be made from step 1.
    expect(button(cs('makePlan'))).toBeInTheDocument();
    await user.click(button(cs('next')));
    expect(wiz().step).toBe('objects');
    await user.click(button(cs('back')));
    expect(wiz().step).toBe('endpoints');
  });

  it('lists same-family targets only, disables those not write-allowed, and marks one write-allowed after a danger confirmation', async () => {
    const user = userEvent.setup();
    const confirm = vi.fn(async (o: ConfirmOptions) => o.title.length > 0);
    setConfirmHandler(confirm);
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' } });
    await user.click(within(dialog()).getByRole('combobox', { name: `${cs('target')} ${cs('connection')}` }));
    const options = await screen.findAllByRole('option');
    // Each option: driver badge, name (+ the driver for screen readers), EnvTag, why it is disabled.
    expect(options.map((o) => o.querySelector('[data-conn-option]')?.getAttribute('data-conn-option'))).toEqual(['c-prod', 'c-staging']);
    expect(options[0]).toHaveTextContent(/^Pgprod \(PostgreSQL\) · PostgreSQLproduction/);
    expect(within(options[0]).getByText('production')).toHaveAttribute('title');
    expect(within(options[1]).getByText('staging')).toBeInTheDocument();
    for (const o of options) expect(o).toHaveTextContent(cs('notWritableNote'));
    expect(options.every((o) => o.getAttribute('aria-disabled') === 'true')).toBe(true);
    await user.keyboard('{Escape}');

    await user.click(within(dialog()).getByRole('button', { name: cs('markWritableMenuAria') }));
    await user.click(await screen.findByRole('menuitem', { name: /staging \(PostgreSQL\)/ }));
    await waitFor(() => expect(confirm).toHaveBeenCalledOnce());
    expect(confirm.mock.calls[0][0]).toMatchObject({ title: cs('markWritableTitle', { name: 'staging (PostgreSQL)' }), danger: true });
    await waitFor(() => expect(wiz().draft.target.connId).toBe('c-staging'));
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-staging')?.writeAllowed).toBe(true);

    // Declining keeps the connection read-only.
    confirm.mockResolvedValueOnce(false);
    await user.click(within(dialog()).getByRole('button', { name: cs('markWritableMenuAria') }));
    await user.click(await screen.findByRole('menuitem', { name: /prod \(PostgreSQL\)/ }));
    await waitFor(() => expect(confirm).toHaveBeenCalledTimes(2));
    expect(confirm.mock.calls[1]?.[0].body).toContain(cs('markWritableProduction'));
    expect(useConnectionsStore.getState().connections.find((c) => c.id === 'c-prod')?.writeAllowed).toBeUndefined();
  });

  it('a preset target that is not write-allowed says so and offers the button', async () => {
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-staging' } });
    expect(await within(dialog()).findByText(cs('selectedNotWritable', { name: 'staging (PostgreSQL)' }))).toBeInTheDocument();
    expect(within(dialog()).getAllByRole('button', { name: cs('markWritable') }).length).toBeGreaterThan(0);
    expect(button(cs('next'))).toBeDisabled();
  });

  it('refuses the same connection and database like core, and warns before writing into production', async () => {
    await allowWrite('c-prod');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-prod', database: 'orders' } });
    expect(await within(dialog()).findByText(cs('sameDatabase'))).toBeInTheDocument();
    expect(button(cs('next'))).toBeDisabled();
    const plan = await copyPlan(wiz().draft);
    expect(plan.blockers.map((b) => b.code)).toContain('SAME_DATABASE');

    open({ source: { connId: 'c-staging', database: 'orders' }, target: { connId: 'c-prod', database: 'analytics_dw' } });
    expect(await within(dialog()).findByText(cs('productionTarget'))).toBeInTheDocument();
    expect(within(dialog()).queryByText(cs('sameDatabase'))).toBeNull();
  });

  it('objects: choose tables with the tree, and the PostgreSQL object switches', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'analytics' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await user.click(stepRadio('objects'));
    expect(within(dialog()).getByText(cs('allTablesNote'))).toBeInTheDocument();
    await user.click(within(dialog()).getByRole('radio', { name: cs('selectionTables') }));
    const daily = await within(dialog()).findByRole('checkbox', { name: cs('tableAria', { name: 'analytics.daily_sales' }) });
    await waitFor(() => expect(daily).toBeChecked());
    // Views are objects, not tables.
    expect(within(dialog()).queryByRole('checkbox', { name: cs('tableAria', { name: 'analytics.v_active_customers' }) })).toBeNull();
    await user.click(within(dialog()).getByRole('button', { name: cs('selectNone') }));
    expect(wiz().draft.selection).toEqual({ kind: 'tables', tables: [] });
    await user.click(daily);
    expect(wiz().draft.selection).toEqual({ kind: 'tables', tables: [{ schema: 'analytics', name: 'daily_sales' }] });
    await user.type(within(dialog()).getByRole('searchbox', { name: cs('searchTables') }), 'churn');
    expect(within(dialog()).queryByRole('checkbox', { name: cs('tableAria', { name: 'analytics.daily_sales' }) })).toBeNull();
    const seq = within(dialog()).getByRole('switch', { name: cs('flag.sequences') });
    expect(seq).toBeChecked();
    expect(within(dialog()).getByRole('switch', { name: cs('flag.owners') })).not.toBeChecked();
    await user.click(seq);
    expect(wiz().draft.options.objects.sequences).toBe(false);
    await makePlan(user);
    expect(wiz().plan!.items.filter((i) => i.kind === 'table').map((i) => i.name)).toEqual(['daily_sales']);
    expect(wiz().plan!.acks.map((a) => a.code)).toContain('SEQUENCES_NOT_SET');
  });

  it('options: skip is the visible default; destructive choices are danger text; the MySQL facts line', async () => {
    const user = userEvent.setup();
    const tgt = await mysql8();
    renderWizard();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await user.click(stepRadio('options'));
    const skip = within(dialog()).getByRole('radio', { name: cs('existing.skip') });
    expect(skip).toBeChecked();
    expect(within(dialog()).getByText(cs('existing.truncate'))).toHaveClass('text-danger');
    expect(within(dialog()).getByText(cs('existing.drop_recreate'))).toHaveClass('text-danger');
    // Before a plan, the version-dependent collation and zero-date policy show as "by target version"
    // (hard-coding utf8mb4_0900_ai_ci would be wrong for pre-8.0 targets).
    expect(within(dialog()).getByTestId('mysql-defaults')).toHaveTextContent(`utf8mb4 · ${cs('versionDefault')} · InnoDB`);
    await user.click(within(dialog()).getByRole('radio', { name: cs('existing.append') }));
    expect(wiz().draft.options.existing).toBe('append');
    // Advanced stays collapsed until asked.
    expect(within(dialog()).queryByRole('combobox', { name: cs('charset') })).toBeNull();
    await user.click(within(dialog()).getByRole('button', { name: cs('advanced') }));
    expect(within(dialog()).getByRole('combobox', { name: cs('charset') })).toBeInTheDocument();
    await user.click(within(dialog()).getByRole('radio', { name: cs('zero.min') }));
    expect(wiz().draft.options.mysql.zeroDates).toBe('min');
    // DESIGN.md "Copy wizard look": the steps bar is the only segmented control; every other
    // choice is a native radio list.
    expect(segmented()).toHaveLength(1);
    for (const name of [cs('content'), cs('verify'), cs('engine'), cs('unmappable'), cs('concurrency')]) {
      const group = within(dialog()).getByRole('radiogroup', { name });
      expect(within(group).getAllByRole('radio').every((r) => r.tagName === 'INPUT')).toBe(true);
    }
    await user.click(within(dialog()).getByRole('radio', { name: cs('engine.myisam') }));
    await user.click(within(dialog()).getByRole('radio', { name: cs('verify.checksum') }));
    await user.click(within(within(dialog()).getByRole('radiogroup', { name: cs('concurrency') })).getByRole('radio', { name: '3' }));
    expect(wiz().draft.options).toMatchObject({ verify: 'checksum', tableConcurrency: 3, mysql: { engine: 'myisam' } });
  });

  it('keyboard: arrows move between steps; plan, acknowledge with Space, start, progress and result', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'orders' } });
    stepRadio('endpoints').focus();
    await user.keyboard('{ArrowRight}');
    expect(wiz().step).toBe('objects');
    await user.keyboard('{ArrowRight}{ArrowRight}');
    expect(wiz().step).toBe('plan');
    button(cs('makePlan')).focus();
    await user.keyboard('{Enter}');
    const ack = await within(dialog()).findByRole('checkbox', { name: cs('acknowledgeAria', { code: 'TABLES_SKIPPED_EXISTING' }) });
    const start = button(cs('startCopy'));
    expect(start).toBeDisabled();
    expect(start.parentElement).toHaveAttribute('title', cs('reason.acks'));
    expect(button(cs('dryRun'))).toBeDisabled();
    expect(within(dialog()).getByTestId('plan-counts')).toHaveTextContent(cs('count.skip', { n: wiz().plan!.totals.skip }));
    ack.focus();
    await user.keyboard(' ');
    expect(ack).toBeChecked();
    expect(start).toBeEnabled();
    start.focus();
    await user.keyboard('{Enter}');
    await waitFor(() => expect(wiz().jobId).toBeTruthy());
    expect(useJobsStore.getState().local[wiz().jobId!]).toBe(true);
    await waitFor(() => expect(wiz().step).toBe('result'), { timeout: 4000 });
    const n = wiz().plan!.items.filter((i) => i.kind === 'table').length;
    expect(within(dialog()).getByText(cs('skippedExisting', { n }))).toBeInTheDocument();
    expect(within(dialog()).getByTestId('result-summary')).toHaveTextContent(cs('sum.skipped', { n }));
    // Esc closes; the job keeps its own state.
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('a production target starts only after its name is typed; typing keeps the plan', async () => {
    const user = userEvent.setup();
    await allowWrite('c-prod');
    renderWizard();
    open({ source: { connId: 'c-staging', database: 'orders', schema: 'analytics' }, target: { connId: 'c-prod', database: 'analytics_dw' } });
    await makePlan(user);
    await ackAll(user);
    const planId = wiz().plan!.planId;
    const start = button(cs('startCopy'));
    expect(start).toBeDisabled();
    expect(start.parentElement).toHaveAttribute('title', cs('reason.production'));
    const input = within(dialog()).getByRole('textbox', { name: cs('productionInput') });
    await user.type(input, 'prod');
    expect(start).toBeDisabled();
    await user.type(input, ' (PostgreSQL)');
    expect(start).toBeEnabled();
    expect(wiz().plan!.planId).toBe(planId);
    await user.click(start);
    await waitFor(() => expect(wiz().step).toBe('progress'));
  });

  it('an item blocker offers [제외], which re-plans without it', async () => {
    const user = userEvent.setup();
    const tgt = await mysql8();
    renderWizard();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await makePlan(user);
    const blockers = within(dialog()).getByRole('list', { name: cs('blockers') });
    expect(blockers).toHaveTextContent(cs('issue.ENUM_DUPLICATE'));
    expect(button(cs('startCopy')).parentElement).toHaveAttribute('title', cs('reason.blockers'));
    expect(within(dialog()).getByTestId('plan-facts')).toHaveTextContent(cs('facts.localInfileOff'));
    expect(within(dialog()).getByText(cs('mysqlVersionsCaption'))).toBeInTheDocument();
    await user.click(within(blockers).getByRole('button', { name: cs('exclude') }));
    await waitFor(() => expect(wiz().plan?.blockers).toEqual([]));
    expect(wiz().draft.options.exclude).toEqual(['table:"products"']);
    expect(within(dialog()).queryByRole('list', { name: cs('blockers') })).toBeNull();
  });

  it('the action menu of an existing table writes an override and re-plans; destructive needs a new acknowledgement', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'orders' } });
    await makePlan(user);
    await ackAll(user);
    expect(button(cs('startCopy'))).toBeEnabled();
    const orders = wiz().plan!.items.find((i) => i.id === 'table:"public"."orders"')!;
    expect(orders.targetHasRows).toBe(true);
    await user.click(within(dialog()).getByRole('button', { name: cs('actionMenu', { name: 'orders' }) }));
    await user.click(await screen.findByRole('menuitem', { name: cs('existing.truncate') }));
    await waitFor(() => expect(wiz().plan?.items.find((i) => i.id === orders.id)?.action).toBe('truncate'));
    expect(wiz().draft.options.existingOverrides).toEqual([{ itemId: orders.id, policy: 'truncate' }]);
    expect(within(dialog()).getByRole('list', { name: cs('warnings') })).toHaveTextContent(cs('issue.DESTRUCTIVE_TRUNCATE'));
    expect(button(cs('startCopy'))).toBeDisabled();
  });

  it('selecting a plan row opens its SQL and source DDL', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await makePlan(user);
    const list = within(dialog()).getByRole('listbox', { name: cs('itemsAria') });
    list.focus();
    await user.keyboard('{ArrowDown}{ArrowDown}');
    const detail = within(dialog()).getByRole('region', { name: cs('detailAria') });
    expect(detail).toHaveTextContent('CREATE TABLE "public"."orders"');
    expect(detail).toHaveTextContent('COPY "public"."orders"');
    await user.click(within(detail).getByRole('tab', { name: cs('detailDdl') }));
    expect(detail).toHaveTextContent('CREATE INDEX idx_orders_customer_id');
    expect(within(dialog()).getByRole('region', { name: cs('factsAria') })).toHaveTextContent('PostgreSQL 16.4');
  });

  it('[중지] asks first; the cancelled result lists what stays in the target and resumes', async () => {
    const user = userEvent.setup();
    mockBackend.configure({ progressTickMs: 25 });
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await makePlan(user);
    await ackAll(user);
    await user.click(button(cs('startCopy')));
    await waitFor(() => expect(within(dialog()).getAllByText(cs('state.loading')).length).toBeGreaterThan(0), { timeout: 4000 });
    expect(within(dialog()).getByRole('progressbar', { name: cs('step.progress') })).toBeInTheDocument();
    await user.click(button(cs('stop')));
    const ask = within(dialog()).getByRole('alertdialog', { name: cs('stopConfirm') });
    await user.click(within(ask).getByRole('button', { name: cs('no') }));
    expect(within(dialog()).queryByRole('alertdialog')).toBeNull();
    await user.click(button(cs('stop')));
    await user.click(within(within(dialog()).getByRole('alertdialog')).getByRole('button', { name: cs('yes') }));
    await waitFor(() => expect(wiz().step).toBe('result'), { timeout: 4000 });
    expect(within(dialog()).getByText(cs('leftBehind'))).toBeInTheDocument();
    expect(within(dialog()).getAllByText(new RegExp(cs('left.created_table'))).length).toBeGreaterThan(0);
    const first = wiz().jobId;
    mockBackend.configure({ progressTickMs: 4 });
    await user.click(button(cs('resume')));
    await waitFor(() => expect(wiz().jobId).not.toBe(first));
    await waitFor(() => expect(wiz().step).toBe('result'), { timeout: 4000 });
    expect(within(dialog()).getByText(cs('resumedPolicy', { policy: cs('existing.skip') }))).toBeInTheDocument();
  });

  it('a dry run says what it checked and [실제로 복사] re-plans', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'analytics' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await makePlan(user);
    await ackAll(user);
    await user.click(button(cs('dryRun')));
    await waitFor(() => expect(wiz().step).toBe('result'), { timeout: 4000 });
    expect(within(dialog()).getByText(cs('dryRun.postgres'))).toBeInTheDocument();
    expect(within(dialog()).queryByRole('button', { name: cs('resume') })).toBeNull();
    await user.click(button(cs('copyForReal')));
    await waitFor(() => expect(wiz().plan).not.toBeNull());
    expect(wiz()).toMatchObject({ jobId: null, step: 'plan' });
    expect(button(cs('startCopy'))).toBeEnabled();
  });

  it('a warned copy job shows its plan read-only and [계속] resumes it with the acknowledged keys', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    const req: CopyRequest = { ...wiz().draft, source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'orders' } };
    const reviewed: CopyPlan = await copyPlan(req);
    // Started with one table's policy changed after the review: core makes it wait in warned.
    const orders = 'table:"public"."orders"';
    const jobId = await copyStart({ ...req, options: { ...req.options, existingOverrides: [{ itemId: orders, policy: 'append' }] } }, reviewed.planId, reviewed.acks.map((a) => a.key));
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.state).toBe('warned'), { timeout: 3000 });
    renderWizard();
    await act(() => store().showJob(jobId));
    expect(within(dialog()).getByText(cs('warnedJob'))).toBeInTheDocument();
    expect(within(dialog()).getByText(cs('planChangedNotice'))).toBeInTheDocument();
    const rows = within(dialog()).getAllByRole('option');
    expect(rows.find((o) => o.getAttribute('data-item') === orders)).toHaveAttribute('data-changed', 'true');
    expect(rows.find((o) => o.getAttribute('data-item') === 'table:"public"."customers"')).not.toHaveAttribute('data-changed');
    // Read-only: no [제외], no action menus, no dry run.
    expect(within(dialog()).queryByRole('button', { name: cs('actionMenu', { name: 'orders' }) })).toBeNull();
    expect(within(dialog()).queryByRole('button', { name: cs('dryRun') })).toBeNull();
    for (const s of ['endpoints', 'objects', 'options']) expect(stepRadio(s)).toHaveAttribute('aria-disabled', 'true');
    const cont = button(cs('continue'));
    expect(cont).toBeDisabled();
    await ackAll(user);
    expect(cont).toBeEnabled();
    await user.click(cont);
    await waitFor(() => expect(mockBackend.calls).toContain('job_resume'));
    await waitFor(() => expect(useJobsStore.getState().jobs[jobId]?.summary.state).not.toBe('warned'));
    await waitFor(() => expect(['progress', 'result']).toContain(wiz().step));
  });

  it('the selection mode is a radio row, not a second segmented control; the connection triggers show the driver badge and EnvTag', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    const source = within(dialog()).getByRole('combobox', { name: `${cs('source')} ${cs('connection')}` });
    await waitFor(() => expect(source.querySelector('[data-conn-option="c-prod"]')).not.toBeNull());
    expect(within(source).getByText('Pg')).toBeInTheDocument();
    expect(within(source).getByText('production')).toBeInTheDocument();
    const target = within(dialog()).getByRole('combobox', { name: `${cs('target')} ${cs('connection')}` });
    expect(within(target).getByText('staging')).toBeInTheDocument();
    await user.click(stepRadio('objects'));
    const mode = within(dialog()).getByRole('radiogroup', { name: cs('selectionAria') });
    // Native radio buttons with labels; the steps above are the segmented control's buttons.
    expect(within(mode).getAllByRole('radio').map((r) => r.tagName)).toEqual(['INPUT', 'INPUT']);
    expect(stepRadio('objects').tagName).toBe('BUTTON');
    await user.click(within(mode).getByRole('radio', { name: cs('selectionTables') }));
    expect(wiz().draft.selection.kind).toBe('tables');
  });

  it('Mod+Enter starts the copy from the plan step, only when [복사 시작] is enabled', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'orders' } });
    await makePlan(user);
    const list = within(dialog()).getByRole('listbox', { name: cs('itemsAria') });
    list.focus();
    // The skip of the existing tables is not acknowledged yet: nothing starts.
    await user.keyboard('{Control>}{Enter}{/Control}');
    expect(wiz().jobId).toBeNull();
    expect(mockBackend.calls).not.toContain('copy_start');
    await ackAll(user);
    list.focus();
    await user.keyboard('{Control>}{Enter}{/Control}');
    await waitFor(() => expect(wiz().jobId).toBeTruthy());
    expect(mockBackend.calls.filter((c) => c === 'copy_start')).toHaveLength(1);
  });

  it('the Activity slot shows the copy counts and [자세히] reopens the wizard on the job', async () => {
    const user = userEvent.setup();
    const showJob = vi.fn(async () => undefined);
    useCopyStore.setState({ showJob });
    const summary: JobSummary = {
      id: 'job-9',
      kind: 'copy',
      state: 'running',
      connId: 'c-prod',
      connName: 'prod',
      label: 'copy',
      startedAt: '2026-10-27T10:00:00Z',
      copy: { runId: 'job-9', targetConnId: 'c-staging', targetConnName: 'staging', sourceDatabase: 'orders', targetDatabase: 'orders', phase: 'data', tablesTotal: 12, tablesDone: 4, tablesFailed: 1, dryRun: true },
    };
    render(<CopyJobSummary summary={summary} />);
    expect(screen.getByText(cs('tables', { done: 4, total: 12 }))).toBeInTheDocument();
    expect(screen.getByText(cs('failed', { n: 1 }))).toBeInTheDocument();
    expect(screen.getByText(cs('dryRunChip'))).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: cs('details') }));
    expect(showJob).toHaveBeenCalledWith('job-9');
  });

  it('the Activity slot counts a job this window never showed from copy:progress, also after it ended', async () => {
    useCopyStore.setState({ showJob: vi.fn(async () => undefined) });
    const summary: JobSummary = {
      id: 'job-elsewhere',
      kind: 'copy',
      state: 'running',
      connId: 'c-prod',
      connName: 'prod',
      label: 'copy',
      startedAt: '2026-10-27T10:00:00Z',
      // As core creates it: no table count until planned.
      copy: { runId: 'job-elsewhere', targetConnId: 'c-staging', targetConnName: 'staging', sourceDatabase: 'orders', targetDatabase: 'orders', phase: 'plan', tablesTotal: 0, tablesDone: 0, tablesFailed: 0, dryRun: false },
    };
    const t = (itemId: string, state: 'loading' | 'failed' | 'done') => ({ seq: BigInt(3), itemId, state, rowsRead: BigInt(1), rowsWritten: BigInt(1), bytes: BigInt(1), rateRowsPerSec: 0 });
    const { rerender } = render(<CopyJobSummary summary={summary} />);
    expect(screen.getByText(cs('tables', { done: 0, total: 0 }))).toBeInTheDocument();
    act(() =>
      store().applyProgress({
        jobId: 'job-elsewhere',
        runId: 'job-elsewhere',
        seq: BigInt(3),
        phase: 'data',
        itemsTotal: 9,
        itemsDone: 4,
        itemsFailed: 3,
        itemsSkipped: 0,
        tablesTotal: 5,
        tablesDone: 2,
        tables: [t('a', 'done'), t('b', 'failed'), t('c', 'loading')],
        full: false,
      }),
    );
    expect(store().runs['job-elsewhere']).toBeUndefined();
    expect(screen.getByText(cs('tables', { done: 2, total: 5 }))).toBeInTheDocument();
    // Failed tables, not every failed item (indexes, keys…).
    expect(screen.getByText(cs('failed', { n: 1 }))).toBeInTheDocument();
    // The job ends; the jobs store still holds the summary it first fetched.
    rerender(<CopyJobSummary summary={{ ...summary, state: 'failed' }} />);
    expect(screen.getByText(cs('tables', { done: 2, total: 5 }))).toBeInTheDocument();
  });

  it('a copy rejected by its start-time plan moves to the plan with its blockers and says why', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'public' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await makePlan(user);
    await ackAll(user);
    // Meanwhile another copy starts writing into the same target (a slow one, still running).
    mockBackend.configure({ progressTickMs: 200 });
    const other = await copyStart(wiz().draft, wiz().plan!.planId, wiz().plan!.acks.map((a) => a.key));
    await waitFor(() => expect(useJobsStore.getState().jobs[other]?.summary.state).toBe('running'), { timeout: 4000 });
    mockBackend.configure({ progressTickMs: 4 });
    await user.click(button(cs('startCopy')));
    // copy_start returned a job; it ends `rejected` (core 5.1), and the wizard does not sit on 진행.
    await waitFor(() => expect(wiz().step).toBe('plan'), { timeout: 4000 });
    const jobId = wiz().jobId!;
    expect(jobId).toBeTruthy();
    expect(useJobsStore.getState().jobs[jobId]?.summary).toMatchObject({ state: 'rejected', error: { code: 'INVALID' } });
    expect(within(dialog()).getByTestId('job-end')).toHaveTextContent(cs('rejectedJob'));
    const blockers = within(dialog()).getByRole('list', { name: cs('blockers') });
    expect(blockers).toHaveTextContent(cs('issue.TARGET_IN_USE'));
    // The 진행 view says why as well (the job's error, one titled line per blocker).
    await user.click(stepRadio('progress'));
    const why = within(dialog()).getByTestId('job-end');
    expect(why).toHaveAttribute('data-state', 'rejected');
    expect(within(why).getByText(cs('issue.TARGET_IN_USE'))).toBeInTheDocument();
    expect(within(dialog()).queryByText(cs('resultMissing'))).toBeNull();
    // The table filter is a native radio row: still one segmented control.
    expect(segmented()).toHaveLength(1);
    // [다시 계획] goes back to editing the same request.
    await user.click(button(cs('replan')));
    await waitFor(() => expect(wiz()).toMatchObject({ jobId: null, step: 'plan' }));
    await waitFor(() => expect(wiz().plan?.blockers.map((b) => b.code)).toContain('TARGET_IN_USE'));
  });

  it('MySQL column findings offer "treat as binary / text", written to mysql.columns, with a localized recommendation', async () => {
    const user = userEvent.setup();
    const tgt = await mysql8();
    renderWizard();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await makePlan(user);
    const base = wiz().plan!;
    const table = base.items.find((i) => i.kind === 'table' && i.name === 'customers')!;
    const binary: CopyPlan['blockers'][number] = {
      code: 'BINARY_ATTR_UNDECODABLE',
      severity: 'danger',
      message: 'binary-attribute column `passwd` holds bytes that are not text',
      recommendation: 'mysql.columns treat_as: binary for `passwd` (varbinary / binary, loaded as bytes)',
      itemId: table.id,
    };
    const blob = (column: string): CopyPlan['blockers'][number] => ({ code: 'BLOB_HOLDS_TEXT', severity: 'info', message: `BLOB column \`${column}\` looks like Korean text`, recommendation: `mysql.columns treat_as: text for \`${column}\`` });
    const plan: CopyPlan = { ...base, blockers: [binary], items: base.items.map((i) => (i.id === table.id ? { ...i, warnings: [...i.warnings, blob('memo'), blob('note')] } : i)) };
    const buildPlan = vi.fn(async () => undefined);
    act(() => useCopyStore.setState((s) => ({ buildPlan, wizard: { ...s.wizard, plan } })));
    const blockers = within(dialog()).getByRole('list', { name: cs('blockers') });
    expect(blockers).toHaveTextContent(cs('rec.BINARY_ATTR_UNDECODABLE', { column: 'passwd' }));
    expect(blockers).not.toHaveTextContent('treat_as');
    await user.click(within(blockers).getByRole('button', { name: cs('treatAs.binary') }));
    expect(wiz().draft.options.mysql.columns).toEqual([{ table: 'customers', column: 'passwd', treatAs: 'binary' }]);
    expect(buildPlan).toHaveBeenCalledTimes(1);
    // The grouped BLOB note lists both columns.
    act(() => useCopyStore.setState((s) => ({ wizard: { ...s.wizard, plan } })));
    await user.click(within(dialog()).getByRole('button', { name: cs('notes', { n: infoIssues(plan).length }) }));
    await user.click(within(dialog()).getByRole('button', { name: cs('treatAsMenu.text') }));
    await user.click(await screen.findByRole('menuitem', { name: cs('treatAsFor', { table: 'customers', column: 'note' }) }));
    expect(wiz().draft.options.mysql.columns).toEqual([
      { table: 'customers', column: 'passwd', treatAs: 'binary' },
      { table: 'customers', column: 'note', treatAs: 'text' },
    ]);
  });
});
