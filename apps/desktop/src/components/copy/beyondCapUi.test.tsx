// Render regression tests for the copy-ui "beyond cap" findings that live in the components:
//  - 주석 is not offered for MySQL (entry 26)
//  - collation choices follow the chosen character set (entry 28)
//  - 모두 해제 respects the search filter, and an empty selection blocks [다음] (entry 29)
//  - the "make the plan" cancel button is named distinctly from the footer close (entry 36)
//  - step 1 target hint for a lone same-family connection, and the write-allowed icon label (entry 37)
//  - resume keeps going after an unlock instead of needing another press (entry 42)
//  - per-table errors and warning samples are reachable without hovering (entry 43)

import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { Connection, CopyResult, CopyStatus, CopyTableProgress, CopyTableResult } from 'ipc-types';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { TooltipProvider } from '@/components/ui';
import { connSave } from '@/lib/commands';
import { setConfirmHandler, type ConfirmOptions } from '@/lib/confirm';
import { subscribeAll } from '@/lib/events';
import { LEGACY_CONN_ID, mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { DEFAULT_COPY_OPTIONS, useCopyStore, type CopyPreset } from '@/stores/copy';
import { useJobsStore } from '@/stores/jobs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { CopyWizard } from './CopyWizard';
import { emptyProgress } from './model';
import { ProgressView } from './ProgressView';
import { ResultView } from './ResultView';
import { cs, k } from './strings';

const wiz = () => useCopyStore.getState().wizard;
const dialog = () => screen.getByRole('dialog');
const button = (name: string) => within(dialog()).getByRole('button', { name });
const stepRadio = (step: string) => within(dialog()).getByRole('radio', { name: cs(k(`step.${step}`)) });

function renderWizard() {
  return render(
    <TooltipProvider>
      <CopyWizard />
    </TooltipProvider>,
  );
}

function open(preset: CopyPreset) {
  act(() => useCopyStore.getState().open(preset));
}

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

beforeEach(async () => {
  resetStores();
  mockBackend.reset();
  mockBackend.configure({ latencyMs: 0, progressTickMs: 4 });
  useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()], drivers: mockDrivers(), loaded: true });
  await subscribeAll({
    onJobState: (ev) => {
      const r = useJobsStore.getState().applyState(ev);
      if (!r.known || r.refetch) void useJobsStore.getState().refreshJob(ev.jobId);
    },
    onJobProgress: (p) => useJobsStore.getState().applyProgress(p),
    onJobDone: (d) => useJobsStore.getState().applyDone(d),
  });
});

describe('entry 25: concurrency help counts the checksum session', () => {
  it('adds one session on each side when 검증 is 체크섬', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await user.click(stepRadio('options'));
    await user.click(within(dialog()).getByRole('radio', { name: cs('verify.checksum') }));
    await user.click(within(dialog()).getByRole('button', { name: cs('advanced') }));
    // Concurrency 1 (PostgreSQL default) + the checksum session: 3 source, 4 target.
    expect(within(dialog()).getByText(cs('concurrencyHelp', { n: 3, m: 4 }))).toBeInTheDocument();
  });
});

describe('entry 27: schema renames are PostgreSQL only', () => {
  it('does not offer the 스키마 rename kind for MySQL', async () => {
    const user = userEvent.setup();
    const tgt = await mysql8();
    renderWizard();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await user.click(stepRadio('options'));
    await user.click(within(dialog()).getByRole('button', { name: cs('advanced') }));
    await user.click(within(dialog()).getByRole('button', { name: cs('renameAdd') }));
    await user.click(within(dialog()).getByRole('combobox', { name: cs('rename') }));
    expect(screen.getByRole('option', { name: cs('renameKind.table') })).toBeInTheDocument();
    expect(screen.queryByRole('option', { name: cs('renameKind.schema') })).toBeNull();
  });
});

describe('entry 35: a dirty draft confirms before it is discarded', () => {
  it('asks before 취소 throws away the chosen options and stays open when declined', async () => {
    const user = userEvent.setup();
    const confirm = vi.fn(async (_o: ConfirmOptions) => false);
    setConfirmHandler(confirm);
    try {
      await allowWrite('c-staging');
      renderWizard();
      open({ source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
      await user.click(stepRadio('options'));
      await user.click(within(dialog()).getByRole('radio', { name: cs('existing.append') }));
      await user.click(button(cs('cancel')));
      expect(confirm).toHaveBeenCalledWith(expect.objectContaining({ title: cs('discardTitle') }));
      // Declined: the wizard is still open.
      expect(wiz().open).toBe(true);
    } finally {
      setConfirmHandler(null);
    }
  });
});

describe('entry 26: MySQL 주석 switch is not offered', () => {
  it('shows 주석 for PostgreSQL but not for MySQL', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'analytics' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await user.click(stepRadio('objects'));
    expect(within(dialog()).getByRole('switch', { name: cs('flag.comments') })).toBeInTheDocument();

    const tgt = await mysql8();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await user.click(stepRadio('objects'));
    expect(within(dialog()).getByRole('switch', { name: cs('flag.triggers') })).toBeInTheDocument();
    expect(within(dialog()).queryByRole('switch', { name: cs('flag.comments') })).toBeNull();
  });
});

describe('entry 28: collation choices follow the character set', () => {
  it('offers utf8mb4 collations for utf8mb4 and only 자동/legacy for another charset', async () => {
    const user = userEvent.setup();
    const tgt = await mysql8();
    renderWizard();
    open({ source: { connId: LEGACY_CONN_ID, database: 'shop' }, target: { connId: tgt.id, database: 'shop_utf8' } });
    await user.click(stepRadio('options'));
    await user.click(within(dialog()).getByRole('button', { name: cs('advanced') }));

    await user.click(within(dialog()).getByRole('combobox', { name: cs('collation') }));
    expect(screen.getAllByRole('option').map((o) => o.textContent)).toContain('utf8mb4_0900_ai_ci');
    await user.keyboard('{Escape}');

    // Switch the character set to euckr: the utf8mb4 collations no longer apply.
    await user.click(within(dialog()).getByRole('combobox', { name: cs('charset') }));
    await user.click(await screen.findByRole('option', { name: 'euckr' }));
    await user.click(within(dialog()).getByRole('combobox', { name: cs('collation') }));
    const after = screen.getAllByRole('option').map((o) => o.textContent);
    expect(after).toEqual([cs('collationAuto'), cs('collationLegacy')]);
  });
});

describe('entry 29: table selection', () => {
  it('모두 해제 clears only the filtered rows, and an empty selection blocks [다음]', async () => {
    const user = userEvent.setup();
    await allowWrite('c-staging');
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders', schema: 'analytics' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    await user.click(stepRadio('objects'));
    await user.click(within(dialog()).getByRole('radio', { name: cs('selectionTables') }));
    const sel = () => wiz().draft.selection as unknown as { kind: string; tables: { name: string }[] };
    await waitFor(() => expect(sel().kind === 'tables' && sel().tables.length > 1).toBe(true));
    const total = sel().tables.length;

    // Filter to the churn table, 모두 해제: only the visible rows leave the selection.
    await user.type(within(dialog()).getByRole('searchbox', { name: cs('searchTables') }), 'churn');
    await user.click(within(dialog()).getByRole('button', { name: cs('selectNone') }));
    const left = sel().tables;
    expect(left.length).toBeGreaterThan(0);
    expect(left.length).toBeLessThan(total);
    expect(left.some((t) => t.name.includes('churn'))).toBe(false);

    // Clear the filter and 모두 해제 everything: the gate message shows and [다음] is blocked.
    await user.clear(within(dialog()).getByRole('searchbox', { name: cs('searchTables') }));
    await user.click(within(dialog()).getByRole('button', { name: cs('selectNone') }));
    // The inline gate message (role=alert) shows, and [다음] is disabled.
    expect(within(dialog()).getByRole('alert')).toHaveTextContent(cs('noTablesSelected'));
    expect(button(cs('next'))).toBeDisabled();
  });
});

describe('entry 36: planning cancel button is named distinctly', () => {
  it('shows 계획 취소 in the body and 취소 in the footer while planning', () => {
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-staging', database: 'analytics_dw' } });
    act(() => useCopyStore.setState((s) => ({ wizard: { ...s.wizard, step: 'plan', planBusy: true, planToken: 't', plan: null, jobId: null } })));
    expect(within(dialog()).getByRole('button', { name: cs('cancelPlan') })).toBeInTheDocument();
    expect(within(dialog()).getByRole('button', { name: cs('cancel') })).toBeInTheDocument();
    expect(cs('cancelPlan')).not.toBe(cs('cancel'));
  });
});

describe('entry 37: step 1 target hints', () => {
  it('a lone same-family PG connection still shows the write-allowed hint, not "no same family"', () => {
    const prod = mockConnections().find((c) => c.id === 'c-prod')!;
    useConnectionsStore.setState({ connections: [prod], drivers: mockDrivers(), loaded: true });
    renderWizard();
    open({ source: { connId: 'c-prod', database: 'orders' } });
    expect(within(dialog()).getByText(cs('targetWritableHint'))).toBeInTheDocument();
    expect(within(dialog()).queryByText(cs('noSameFamily', { driver: 'PostgreSQL' }))).toBeNull();
  });

  it('labels the write-allowed icon with the write-allowed tip', async () => {
    await allowWrite('c-prod');
    renderWizard();
    open({ source: { connId: 'c-staging', database: 'orders' }, target: { connId: 'c-prod', database: 'analytics_dw' } });
    expect(await within(dialog()).findByLabelText(cs('writeAllowedTip'))).toBeInTheDocument();
  });
});

// ---- direct component renders (resume and accessibility) ----

function lockedConn(): Connection {
  const prod = mockConnections().find((c) => c.id === 'c-prod')!;
  return { ...prod, id: 'c-ask', name: 'ask', savePassword: false, hasPassword: false };
}

function resumableStatus(tables: CopyTableResult[] = []): CopyStatus {
  const result: CopyResult = {
    runId: 'r1',
    dryRun: false,
    existing: 'skip',
    tables,
    objects: [],
    warnings: [],
    leftBehind: [],
    verified: false,
    problems: 0,
    skippedExisting: 0,
    resumable: true,
    durationMs: BigInt(0),
  };
  return { jobId: 'j1', runId: 'r1', state: 'failed', progress: emptyProgress('j1', 'r1'), result };
}

describe('entry 42: resume continues after an unlock', () => {
  it('asks for the locked side with a `then` that resumes, instead of needing a second press', async () => {
    const user = userEvent.setup();
    useConnectionsStore.setState({ connections: [lockedConn(), ...mockConnections().filter((c) => c.id !== 'c-prod')], drivers: mockDrivers(), loaded: true });
    useCopyStore.setState((s) => ({ runs: { j1: resumableStatus() }, wizard: { ...s.wizard, draft: { source: { connId: 'c-ask', database: 'orders' }, target: { connId: 'c-staging', database: 'analytics_dw' }, selection: { kind: 'all' }, options: structuredClone(DEFAULT_COPY_OPTIONS) } } }));
    const ask = vi.fn();
    useUiStore.setState({ askDbPassword: ask });

    render(
      <TooltipProvider>
        <ResultView status={useCopyStore.getState().runs.j1} />
      </TooltipProvider>,
    );
    await user.click(screen.getByRole('button', { name: cs('resume') }));
    expect(ask).toHaveBeenCalledWith('c-ask', 'browse', expect.objectContaining({ then: expect.any(Function) }));
  });
});

describe('entry 43: per-table errors and warning samples are not hover-only', () => {
  it('ProgressView renders a failed table error inline', () => {
    const t: CopyTableProgress = {
      seq: BigInt(1),
      itemId: 'table:"public"."orders"',
      state: 'failed',
      rowsRead: BigInt(0),
      rowsWritten: BigInt(0),
      bytes: BigInt(0),
      rateRowsPerSec: 0,
      error: { code: 'QUERY', message: 'relation does not exist' },
    };
    const status: CopyStatus = { jobId: 'j1', runId: 'r1', state: 'running', progress: { ...emptyProgress('j1', 'r1'), phase: 'data', tablesTotal: 1, tables: [t] } };
    render(
      <TooltipProvider>
        <ProgressView status={status} />
      </TooltipProvider>,
    );
    const inline = document.querySelector('[data-error]');
    expect(inline?.textContent).toContain('relation does not exist');
  });

  it('ResultView exposes the verification warning samples on a focusable cell', () => {
    const tr: CopyTableResult = {
      itemId: 'table:"public"."orders"',
      targetName: 'orders',
      state: 'done',
      rowsRead: BigInt(10),
      rowsWritten: BigInt(10),
      targetCount: BigInt(10),
      verify: 'lossy',
      unmappableChars: BigInt(0),
      dataWarnings: BigInt(2),
      warningSamples: ['col=0xAB', 'col=0xCD'],
      valueFixes: [],
      problems: [],
      durationMs: BigInt(1),
    };
    render(
      <TooltipProvider>
        <ResultView status={resumableStatus([tr])} />
      </TooltipProvider>,
    );
    // The accessible name carries the samples; query the attribute directly (getByLabelText
    // collapses the newline between samples, so match on the raw value instead).
    const cell = document.querySelector('td [tabindex="0"]');
    expect(cell).not.toBeNull();
    const label = cell!.getAttribute('aria-label') ?? '';
    expect(label).toContain('col=0xAB');
    expect(label).toContain('col=0xCD');
  });
});
