import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useLogsStore } from '@/stores/logs';
import { resetStores } from '@/test/stores';
import { openUrl } from './dialogs';

describe('openUrl', () => {
  beforeEach(() => resetStores());
  afterEach(() => vi.restoreAllMocks());

  it('hands the URL to the browser and reports success with null', async () => {
    const open = vi.spyOn(window, 'open').mockImplementation(() => null);
    await expect(openUrl('https://example.com/odbc')).resolves.toBeNull();
    expect(open).toHaveBeenCalledWith('https://example.com/odbc', '_blank', 'noopener');
    expect(useLogsStore.getState().entries).toHaveLength(0);
  });

  it('never rejects: a failure is logged and returned for an inline notice', async () => {
    vi.spyOn(window, 'open').mockImplementation(() => {
      throw new Error('pop-up blocked');
    });
    const message = await openUrl('https://example.com/oracle');
    expect(message).toBe('Could not open https://example.com/oracle: pop-up blocked');
    const entries = useLogsStore.getState().entries;
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({ level: 'error', message, target: 'ui' });
    expect(useLogsStore.getState().errorFlag).toBe(true);
  });

  it('keeps the reason of a plugin error that arrives as a plain string', async () => {
    vi.spyOn(window, 'open').mockImplementation(() => {
      throw 'Not allowed to open url ftp://x';
    });
    await expect(openUrl('ftp://x')).resolves.toBe('Could not open ftp://x: Not allowed to open url ftp://x');
  });
});
