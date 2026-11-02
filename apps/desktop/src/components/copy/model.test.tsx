// Pure helpers of the copy wizard (model.ts) and the result view's rendering of a run that ended
// with problems (value fixes per column, unmappable characters, what stays behind, resume).

import { render, screen } from '@testing-library/react';
import type { CopyPlan, CopyRequest, CopyStatus } from 'ipc-types';
import { beforeEach, describe, expect, it } from 'vitest';
import { TooltipProvider } from '@/components/ui';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { useConnectionsStore } from '@/stores/connections';
import { DEFAULT_COPY_OPTIONS, defaultCopyOptions, useCopyStore } from '@/stores/copy';
import { resetStores } from '@/test/stores';
import { emptyProgress, endpointProblem, factsEndpoint, isDefaultOptions, issueTitle, planChangedIds, startBlock } from './model';
import { ResultView } from './ResultView';
import { cs } from './strings';

const conns = () => mockConnections().map((c) => (c.id === 'c-staging' ? { ...c, writeAllowed: true } : c));

function draft(over: Partial<CopyRequest> = {}): CopyRequest {
  return { source: { connId: 'c-prod', database: 'orders' }, target: { connId: 'c-staging', database: 'orders' }, selection: { kind: 'all' }, options: defaultCopyOptions(), ...over };
}

describe('copy model', () => {
  it('endpointProblem names the first thing missing', () => {
    expect(endpointProblem(draft(), conns())).toBeNull();
    expect(endpointProblem(draft({ source: { connId: '', database: '' } }), conns())).toBe('sourceConn');
    expect(endpointProblem(draft({ source: { connId: 'c-prod', database: ' ' } }), conns())).toBe('sourceDb');
    expect(endpointProblem(draft({ target: { connId: 'c-local', database: 'main' } }), conns())).toBe('family');
    expect(endpointProblem(draft(), mockConnections())).toBe('notWritable');
    expect(endpointProblem(draft({ target: { connId: 'c-staging', database: '' } }), conns())).toBe('targetDb');
    const same = conns().map((c) => (c.id === 'c-prod' ? { ...c, writeAllowed: true } : c));
    expect(endpointProblem(draft({ target: { connId: 'c-prod', database: 'orders', schema: 'other' } }), same)).toBe('sameDatabase');
  });

  it('startBlock checks plan, write access, blockers, acknowledgements and the production name in order', () => {
    const plan = { blockers: [], acks: [{ code: 'X', key: 'X#1', severity: 'warning', items: [] }] } as unknown as CopyPlan;
    const w = { plan, planBusy: false, draft: draft(), acks: [] as string[], confirmName: '' };
    expect(startBlock({ ...w, plan: null }, conns())).toBe('noPlan');
    expect(startBlock({ ...w, planBusy: true }, conns())).toBe('planBusy');
    expect(startBlock(w, mockConnections())).toBe('notWritable');
    expect(startBlock({ ...w, plan: { ...plan, blockers: [{ code: 'B', severity: 'danger', message: 'b' }] } }, conns())).toBe('blockers');
    expect(startBlock(w, conns())).toBe('acks');
    expect(startBlock({ ...w, acks: ['X#1'] }, conns())).toBeNull();
    const prod = conns().map((c) => (c.id === 'c-staging' ? { ...c, tag: 'production' as const } : c));
    expect(startBlock({ ...w, acks: ['X#1'] }, prod)).toBe('production');
    expect(startBlock({ ...w, acks: ['X#1'], confirmName: 'staging (PostgreSQL) ' }, prod)).toBe('production');
    expect(startBlock({ ...w, acks: ['X#1'], confirmName: 'staging (PostgreSQL)' }, prod)).toBeNull();
  });

  it('formats endpoints, titles issues and recognises the defaults', () => {
    expect(factsEndpoint({ driver: 'postgres', endpoint: 'db:5432 via ssh bastion', database: 'orders' }, 'app')).toBe('db:5432/orders.app via ssh bastion');
    expect(factsEndpoint({ driver: 'sqlite', endpoint: 'C:\\a.db', database: 'main' })).toBe('C:\\a.db');
    expect(issueTitle({ code: 'SAME_DATABASE', message: 'm' })).toBe(cs('issue.SAME_DATABASE'));
    expect(issueTitle({ code: 'SOMETHING_NEW', message: 'English fallback' })).toBe('English fallback');
    expect(isDefaultOptions(defaultCopyOptions())).toBe(true);
    expect(isDefaultOptions({ ...DEFAULT_COPY_OPTIONS, batchRows: undefined })).toBe(true);
    expect(isDefaultOptions({ ...DEFAULT_COPY_OPTIONS, existing: 'append' })).toBe(false);
    const plan = { items: [{ id: 'table:"a"', warnings: [] }, { id: 'table:"b"', warnings: [] }], warnings: [{ code: 'PLAN_CHANGED', severity: 'warning', message: '', detail: 'table:"b"' }] } as unknown as CopyPlan;
    expect([...planChangedIds(plan)]).toEqual(['table:"b"']);
  });
});

describe('ResultView', () => {
  beforeEach(() => {
    resetStores();
    useConnectionsStore.setState({ connections: conns(), drivers: mockDrivers(), loaded: true });
  });

  it('shows problems, value fixes per column, unmappable characters, what stays behind and resume', () => {
    useCopyStore.setState((s) => ({ wizard: { ...s.wizard, draft: draft() } }));
    const status: CopyStatus = {
      jobId: 'j',
      runId: 'j',
      state: 'failed',
      progress: emptyProgress('j', 'j'),
      result: {
        runId: 'j',
        dryRun: false,
        existing: 'skip',
        tables: [
          {
            itemId: 'table:"orders"',
            targetName: 'orders',
            state: 'done',
            rowsRead: BigInt(10),
            rowsWritten: BigInt(10),
            targetCount: BigInt(10),
            verify: 'lossy',
            unmappableChars: BigInt(3),
            dataWarnings: BigInt(0),
            warningSamples: [],
            valueFixes: [
              { column: 'created_at', code: 'ZERO_DATE_NULL', count: BigInt(12) },
              { column: 'created_at', code: 'INVALID_DATE', count: BigInt(3) },
            ],
            problems: ['UNMAPPABLE_CHARS', 'INVALID_DATE'],
            durationMs: BigInt(5),
          },
          {
            itemId: 'table:"users"',
            targetName: 'users',
            state: 'cancelled',
            rowsRead: BigInt(4),
            rowsWritten: BigInt(4),
            verify: 'not_run',
            unmappableChars: BigInt(0),
            dataWarnings: BigInt(0),
            warningSamples: [],
            valueFixes: [],
            problems: [],
            durationMs: BigInt(5),
          },
        ],
        objects: [],
        warnings: [{ code: 'UNMAPPABLE_CHARS', severity: 'warning', message: '3 characters could not be converted' }],
        leftBehind: [
          { itemId: 'table:"users"', kind: 'partial_rows', rows: BigInt(4) },
          { itemId: 'table:"users"', kind: 'missing_post_data' },
        ],
        verified: false,
        problems: 1,
        skippedExisting: 0,
        resumable: true,
        durationMs: BigInt(2000),
      },
    };
    render(
      <TooltipProvider>
        <ResultView status={status} />
      </TooltipProvider>,
    );
    expect(screen.getByText(cs('problemsBanner', { n: 1 }))).toBeInTheDocument();
    expect(screen.getByText(`created_at: ${cs('fix.ZERO_DATE_NULL', { n: '12' })}, ${cs('fix.INVALID_DATE', { n: '3' })}`)).toBeInTheDocument();
    expect(screen.getByText(cs('verify.lossy'))).toBeInTheDocument();
    expect(screen.getByText(cs('unmappableChars', { n: '3' }))).toBeInTheDocument();
    expect(screen.getByText(`table:"users": ${cs('left.partial_rows', { rows: '4' })}`)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: cs('resume') })).toBeEnabled();
    expect(screen.getByRole('button', { name: cs('clearWritable') })).toBeInTheDocument();
  });
});
