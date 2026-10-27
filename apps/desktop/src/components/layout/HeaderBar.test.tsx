import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterAll, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { setLang } from '@/lib/i18n';
import { mockConnections, mockDrivers } from '@/lib/mock-data';
import { mockBackend } from '@/lib/mock-ipc';
import { useConnectionsStore } from '@/stores/connections';
import { resetStores } from '@/test/stores';
import { TooltipProvider } from '@/components/ui';
import { HeaderBar } from './HeaderBar';

beforeAll(() => setLang('en'));
afterAll(() => setLang('ko'));

describe('HeaderBar', () => {
  beforeEach(() => {
    resetStores();
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    useConnectionsStore.setState({ connections: mockConnections(), drivers: mockDrivers(), selectedId: 'c-prod', loaded: true });
  });

  it('opens the About dialog with the app info', async () => {
    const user = userEvent.setup();
    render(
      <TooltipProvider>
        <HeaderBar />
      </TooltipProvider>,
    );
    expect(screen.queryByRole('dialog')).toBeNull();
    await user.click(screen.getByRole('button', { name: 'About BulkDB' }));
    expect(await screen.findByRole('dialog', { name: 'About BulkDB' })).toBeInTheDocument();
    expect(await screen.findByText('0.1.0-mock')).toBeInTheDocument();
    // The connections store already had the drivers: no extra request.
    expect(screen.getByText('5 of 6 available')).toBeInTheDocument();
    expect(mockBackend.calls).not.toContain('drivers_list');
  });
});
