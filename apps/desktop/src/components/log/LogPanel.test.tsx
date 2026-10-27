import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it } from 'vitest';
import { t } from '@/lib/i18n';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { LogPanel } from './LogPanel';

const at = '2026-10-26T10:00:00.000Z';

describe('LogPanel', () => {
  beforeEach(() => {
    resetStores();
    useUiStore.setState({ logOpen: false });
  });

  it('stays collapsed on info logs and auto-expands on an error', async () => {
    render(
      <TooltipProvider>
        <LogPanel />
      </TooltipProvider>,
    );
    const name = new RegExp(t('log.title'));
    expect(screen.getByRole('button', { name, expanded: false })).toBeInTheDocument();

    act(() => useLogsStore.getState().push({ level: 'info', message: 'hello', at }));
    expect(screen.queryByText('hello')).not.toBeInTheDocument();
    expect(useUiStore.getState().logOpen).toBe(false);

    act(() => useLogsStore.getState().push({ level: 'error', message: 'COPY failed: connection reset', at }));
    expect(useUiStore.getState().logOpen).toBe(true);
    expect(await screen.findByText('COPY failed: connection reset')).toBeInTheDocument();
    expect(screen.getByRole('button', { name, expanded: true })).toBeInTheDocument();
    expect(screen.getByText('hello')).toBeInTheDocument();
    expect(useLogsStore.getState().errorCount).toBe(1);
  });

  it('filters by level', () => {
    useUiStore.setState({ logOpen: true });
    useLogsStore.getState().pushMany([
      { level: 'debug', message: 'dbg', at },
      { level: 'info', message: 'inf', at },
      { level: 'warn', message: 'wrn', at },
    ]);
    render(
      <TooltipProvider>
        <LogPanel />
      </TooltipProvider>,
    );
    expect(screen.queryByText('dbg')).not.toBeInTheDocument();
    expect(screen.getByText('inf')).toBeInTheDocument();
    act(() => useLogsStore.getState().setFilter('warn'));
    expect(screen.queryByText('inf')).not.toBeInTheDocument();
    expect(screen.getByText('wrn')).toBeInTheDocument();
  });
});
