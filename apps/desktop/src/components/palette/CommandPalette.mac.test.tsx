import { act, render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { registerBuiltinActions } from '@/lib/actions';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { useConnectionsStore } from '@/stores/connections';
import { useUiStore } from '@/stores/ui';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { CommandPalette } from './CommandPalette';
import { ps } from './strings';

// macOS: Mod = Cmd, so ⌘P toggles the palette while Ctrl+N / Ctrl+P keep moving.
vi.mock('@/lib/platform', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/platform')>();
  return { ...actual, isMac: () => true, hasModKey: (e: { metaKey: boolean }) => e.metaKey };
});

describe('CommandPalette on macOS', () => {
  beforeEach(() => {
    resetStores();
    registerBuiltinActions();
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), loaded: true });
  });

  it('moves with Ctrl+N / Ctrl+P and closes with ⌘P', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <CommandPalette />
      </TooltipProvider>,
    );
    act(() => useUiStore.getState().openPalette('connections'));
    const input = screen.getByRole('combobox', { name: ps('title') });
    const options = () => screen.getAllByRole('option');
    await user.keyboard('{Control>}n{/Control}');
    expect(input).toHaveAttribute('aria-activedescendant', options()[1].id);
    await user.keyboard('{Control>}p{/Control}');
    expect(input).toHaveAttribute('aria-activedescendant', options()[0].id);
    await user.keyboard('{Meta>}p{/Meta}');
    expect(useUiStore.getState().palette.open).toBe(false);
  });
});
