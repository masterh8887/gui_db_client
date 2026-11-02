// Export panel 출력: the CSV encoding control (docs/mysql-compat.md §6), the BOM interplay, the
// Excel preset, Parquet; the SQL_ENCODING blocker in the preflight sheet; the replaced-characters
// note of a finished EUC-KR job.

import { act, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it } from 'vitest';
import { exportPreflight } from '@/lib/commands';
import { LEGACY_CONN_ID, mockConnections, mockDrivers, mockLegacyConnection } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { TooltipProvider } from '@/components/ui';
import { useConnectionsStore } from '@/stores/connections';
import { useExtractStore } from '@/stores/extract';
import { resetStores } from '@/test/stores';
import { parseUnmappableNote } from './jobNotes';
import { OutputOptionsForm } from './OutputOptionsForm';
import { PreflightSheet } from './PreflightSheet';
import { qs } from './strings';

const ex = () => useExtractStore.getState();
const csv = () => ex().docs.q1.output.csv!;
const encodingRadio = (name: string) => within(screen.getByRole('radiogroup', { name: qs('encoding') })).getByRole('radio', { name });
const bom = () => screen.getByRole('checkbox', { name: qs('bom') });

function renderForm() {
  return render(
    <TooltipProvider>
      <OutputOptionsForm tabId="q1" />
    </TooltipProvider>,
  );
}

describe('export panel: CSV encoding', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()], drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
    ex().createDoc('q1', 'c-prod', { sql: 'SELECT * FROM public.orders' });
  });

  it('defaults to UTF-8 and builds a request without an encoding key', () => {
    renderForm();
    expect(encodingRadio(qs('encodingUtf8'))).toHaveAttribute('aria-checked', 'true');
    expect(bom()).toBeEnabled();
    expect(ex().buildRequest('q1')?.output.csv).not.toHaveProperty('encoding');
  });

  it('EUC-KR disables and clears the BOM with a hint; back to UTF-8 drops the key', async () => {
    const user = userEvent.setup();
    renderForm();
    await user.click(bom());
    expect(csv().bom).toBe(true);

    await user.click(encodingRadio(qs('encodingEucKr')));
    expect(bom()).toBeDisabled();
    expect(bom()).not.toBeChecked();
    expect(screen.getByText(qs('encodingHint'))).toBeInTheDocument();
    expect(csv()).toMatchObject({ encoding: 'euc-kr', bom: false });
    expect(ex().buildRequest('q1')?.output.csv).toMatchObject({ encoding: 'euc-kr', bom: false });

    await user.click(encodingRadio(qs('encodingUtf8')));
    expect(bom()).toBeEnabled();
    expect(csv()).not.toHaveProperty('encoding');
    expect(ex().buildRequest('q1')?.output.csv).not.toHaveProperty('encoding');
  });

  it('a request never carries a BOM with EUC-KR, even from options set elsewhere (history, legacy setCsv)', () => {
    act(() => ex().setCsv('q1', { encoding: 'euc-kr', bom: true }));
    expect(ex().buildRequest('q1')?.output.csv).toMatchObject({ encoding: 'euc-kr', bom: false });
    act(() => ex().setCsv('q1', { encoding: 'utf-8', bom: true }));
    const out = ex().buildRequest('q1')?.output.csv;
    expect(out).toMatchObject({ bom: true });
    expect(out).not.toHaveProperty('encoding');
  });

  it('the Excel preset keeps UTF-8 + BOM', async () => {
    const user = userEvent.setup();
    renderForm();
    await user.click(encodingRadio(qs('encodingEucKr')));
    await user.click(screen.getByRole('button', { name: qs('excelPreset') }));
    expect(encodingRadio(qs('encodingUtf8'))).toHaveAttribute('aria-checked', 'true');
    expect(bom()).toBeChecked();
    expect(csv()).not.toHaveProperty('encoding');
    expect(csv().bom).toBe(true);
  });

  it('Parquet shows the encoding disabled with the always-UTF-8 hint and sends no CSV options', async () => {
    const user = userEvent.setup();
    renderForm();
    await user.click(encodingRadio(qs('encodingEucKr')));
    await user.click(screen.getByRole('radio', { name: 'Parquet' }));
    expect(encodingRadio(qs('encodingUtf8'))).toHaveAttribute('aria-disabled', 'true');
    expect(encodingRadio(qs('encodingEucKr'))).toHaveAttribute('aria-disabled', 'true');
    expect(screen.getByText(qs('encodingParquet'))).toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: qs('bom') })).toBeNull();
    const req = ex().buildRequest('q1');
    expect(req?.output.format).toBe('parquet');
    expect(req?.output.csv).toBeUndefined();
  });
});

describe('preflight sheet: SQL_ENCODING', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: [...mockConnections(), mockLegacyConnection()], drivers: mockDrivers(), selectedId: LEGACY_CONN_ID, loaded: true });
  });

  it('shows the blocker of an euc-kr connection with the character and its line / column, and cannot start', async () => {
    ex().createDoc('q1', LEGACY_CONN_ID, { sql: "SELECT *\nFROM orders WHERE memo = '😀'" });
    const req = ex().buildRequest('q1')!;
    const report = await exportPreflight(req);
    render(
      <TooltipProvider>
        <PreflightSheet />
      </TooltipProvider>,
    );
    act(() => useExtractStore.setState({ preflightDialog: { kind: 'run', tabId: 'q1', req, report, busy: false, error: null } }));
    const blockers = within(screen.getByRole('dialog')).getByRole('list', { name: qs('blockers') });
    const row = blockers.querySelector('[data-code="SQL_ENCODING"]') as HTMLElement;
    expect(row).not.toBeNull();
    expect(row).toHaveTextContent("'😀' (U+1F600) at line 2, column 27");
    expect(row).toHaveTextContent('line 2, column 27');
    expect(within(row).queryByRole('checkbox')).toBeNull();
    expect(screen.getByRole('button', { name: qs('start') })).toBeDisabled();
  });
});

describe('job result notes', () => {
  it('recognizes the replaced-characters note of the core', () => {
    expect(parseUnmappableNote("12 character(s) cannot be represented in EUC-KR (CP949) and were written as '?'")).toEqual({
      count: BigInt(12),
      encoding: 'EUC-KR (CP949)',
    });
    expect(parseUnmappableNote('CSV header written')).toBeNull();
  });
});
