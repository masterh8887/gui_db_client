import { act, fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { ConsoleDrawer } from './ConsoleDrawer';
import { LogStatusButton } from './LogStatusButton';
import { cs } from './strings';

const at = '2026-10-26T10:00:00.000Z';

function renderDrawer(onClose = vi.fn()) {
  render(
    <TooltipProvider>
      <ConsoleDrawer onClose={onClose} />
    </TooltipProvider>,
  );
  return onClose;
}

const log = () => screen.getByRole('log', { name: cs('title') });

describe('ConsoleDrawer', () => {
  beforeEach(() => {
    resetStores();
    useUiStore.setState({ logOpen: false });
  });

  it('renders rows with time, level, job id, target and message', () => {
    useLogsStore.getState().pushMany([
      { level: 'info', message: 'COPY started', at, jobId: 'job-7', target: 'bulkdb::job' },
      { level: 'warn', message: 'slow consumer', at },
    ]);
    renderDrawer();
    const rows = within(log()).getAllByText(/COPY started|slow consumer/).map((el) => el.parentElement!);
    expect(rows[0]).toHaveAttribute('data-level', 'info');
    expect(within(rows[0]).getByText('[job-7]')).toBeInTheDocument();
    expect(within(rows[0]).getByText('bulkdb::job')).toBeInTheDocument();
    expect(within(rows[0]).getByText('info')).toBeInTheDocument();
    expect(rows[1]).toHaveAttribute('data-level', 'warn');
    expect(screen.getByLabelText(cs('count', { n: 2 }))).toHaveTextContent('2');
  });

  it('auto-opens on an error (logs store) and tints error rows', () => {
    renderDrawer();
    act(() => useLogsStore.getState().push({ level: 'info', message: 'hello', at }));
    expect(useUiStore.getState().logOpen).toBe(false);
    act(() => useLogsStore.getState().push({ level: 'error', message: 'COPY failed: connection reset', at }));
    expect(useUiStore.getState().logOpen).toBe(true);
    const row = screen.getByText('COPY failed: connection reset').parentElement!;
    expect(row).toHaveAttribute('data-level', 'error');
    expect(row.className).toContain('bg-danger-soft');
    expect(screen.getByText(cs('errors', { n: 1 }))).toBeInTheDocument();
  });

  it('filters by level', () => {
    useLogsStore.getState().pushMany([
      { level: 'debug', message: 'dbg', at },
      { level: 'info', message: 'inf', at },
      { level: 'warn', message: 'wrn', at },
    ]);
    renderDrawer();
    expect(screen.queryByText('dbg')).not.toBeInTheDocument();
    expect(screen.getByText('inf')).toBeInTheDocument();
    expect(screen.getByRole('combobox', { name: cs('level') })).toHaveTextContent('info');
    act(() => useLogsStore.getState().setFilter('warn'));
    expect(screen.queryByText('inf')).not.toBeInTheDocument();
    expect(screen.getByText('wrn')).toBeInTheDocument();
  });

  it('clears the console', async () => {
    const user = userEvent.setup();
    useLogsStore.getState().push({ level: 'error', message: 'boom', at });
    renderDrawer();
    await user.click(screen.getByRole('button', { name: cs('clear') }));
    expect(useLogsStore.getState().entries).toHaveLength(0);
    expect(useLogsStore.getState().errorCount).toBe(0);
    expect(screen.getByText(cs('empty'))).toBeInTheDocument();
  });

  it('closes with the close button and with Esc inside the drawer', async () => {
    const user = userEvent.setup();
    const onClose = renderDrawer();
    await user.click(screen.getByRole('button', { name: cs('close') }));
    expect(onClose).toHaveBeenCalledTimes(1);
    const ev = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true });
    log().dispatchEvent(ev);
    expect(onClose).toHaveBeenCalledTimes(2);
    // Consumed: the global Esc (ask to cancel a job) never sees it.
    expect(ev.defaultPrevented).toBe(true);
  });

  it('sticks to the bottom unless the user scrolled up', () => {
    useLogsStore.getState().push({ level: 'info', message: 'one', at });
    renderDrawer();
    const el = log();
    Object.defineProperty(el, 'scrollHeight', { configurable: true, get: () => 1000 });
    Object.defineProperty(el, 'clientHeight', { configurable: true, get: () => 200 });
    act(() => useLogsStore.getState().push({ level: 'info', message: 'two', at }));
    expect(el.scrollTop).toBe(1000);
    el.scrollTop = 100;
    fireEvent.scroll(el);
    act(() => useLogsStore.getState().push({ level: 'info', message: 'three', at }));
    expect(el.scrollTop).toBe(100);
  });
});

describe('LogStatusButton', () => {
  beforeEach(() => {
    resetStores();
    useUiStore.setState({ logOpen: false });
  });

  function renderButton() {
    render(
      <TooltipProvider>
        <LogStatusButton />
      </TooltipProvider>,
    );
  }

  it('toggles the console and shows no badge without errors', async () => {
    const user = userEvent.setup();
    renderButton();
    const button = screen.getByRole('button', { name: cs('button') });
    expect(button).toHaveAttribute('aria-pressed', 'false');
    expect(screen.queryByTestId('console-error-badge')).not.toBeInTheDocument();
    await user.click(button);
    expect(useUiStore.getState().logOpen).toBe(true);
    expect(button).toHaveAttribute('aria-pressed', 'true');
    await user.click(button);
    expect(useUiStore.getState().logOpen).toBe(false);
  });

  it('shows the error count solid until acknowledged, then at 50%', async () => {
    const user = userEvent.setup();
    renderButton();
    act(() => {
      useLogsStore.getState().push({ level: 'error', message: 'a', at });
      useLogsStore.getState().push({ level: 'error', message: 'b', at });
    });
    const button = screen.getByRole('button', { name: cs('buttonErrors', { n: 2 }) });
    const badge = screen.getByTestId('console-error-badge');
    expect(badge).toHaveTextContent('2');
    expect(badge.className).not.toContain('opacity-50');
    // The error auto-opened the console; clicking closes it and acknowledges.
    expect(useUiStore.getState().logOpen).toBe(true);
    await user.click(button);
    expect(useLogsStore.getState().errorFlag).toBe(false);
    expect(useUiStore.getState().logOpen).toBe(false);
    expect(screen.getByTestId('console-error-badge').className).toContain('opacity-50');
  });
});
