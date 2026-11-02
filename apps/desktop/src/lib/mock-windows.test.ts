import type { InstancesInfo, WindowInfo, WindowInit } from 'ipc-types';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { invoke, listen } from './ipc';
import { isIpcError } from './ipc-error';
import { mockBackend } from './mock-ipc';
import { createWindowLabels, MOCK_MAX_WINDOWS, MOCK_WINDOW_LABEL, mockWindowTitle } from './mock-windows';

async function code(p: Promise<unknown>): Promise<string> {
  try {
    await p;
    return 'ok';
  } catch (e) {
    return isIpcError(e) ? e.code : 'other';
  }
}

describe('mock windows', () => {
  let changes: WindowInfo[][];
  beforeEach(async () => {
    window.history.replaceState(null, '', '/');
    mockBackend.reset();
    mockBackend.configure({ latencyMs: 0 });
    changes = [];
    await listen<WindowInfo[]>('windows:changed', (ws) => changes.push(ws));
  });
  afterEach(() => window.history.replaceState(null, '', '/'));

  it('starts as window 1 on Welcome', async () => {
    expect(await invoke<WindowInit>('window_init')).toEqual({ label: MOCK_WINDOW_LABEL, index: 1 });
    expect(await invoke<WindowInfo[]>('window_list')).toEqual([{ label: 'main', title: 'BulkDB', focused: true, index: 1 }]);
    expect(mockWindowTitle('prod')).toBe('BulkDB — prod');
  });

  it('plays the window named in the URL and opens its connection once', async () => {
    window.history.replaceState(null, '', '/?window=w-3&index=2&conn=c-prod');
    mockBackend.reset();
    expect(await invoke<WindowInit>('window_init')).toEqual({ label: 'w-3', index: 2, openConnId: 'c-prod' });
    expect(await invoke<WindowInit>('window_init')).toEqual({ label: 'w-3', index: 2 });
  });

  it('window_new simulates a window showing its connection; at most 8', async () => {
    const label = await invoke<string>('window_new', { connId: 'c-prod' });
    expect(label).toBe('w-2');
    await Promise.resolve();
    const last = changes.at(-1) ?? [];
    expect(last.map((w) => [w.label, w.index, w.connId, w.title])).toEqual([
      ['main', 1, undefined, 'BulkDB'],
      ['w-2', 2, 'c-prod', 'BulkDB — prod (PostgreSQL)'],
    ]);
    expect(await code(invoke('window_new', { connId: 'nope' }))).toBe('NOT_FOUND');
    for (let i = 2; i < MOCK_MAX_WINDOWS; i++) await invoke('window_new');
    expect((await invoke<WindowInfo[]>('window_list')).length).toBe(MOCK_MAX_WINDOWS);
    expect(await code(invoke('window_new'))).toBe('INVALID');
  });

  it('window_focus and window_set_connection', async () => {
    await invoke('window_new');
    expect(await code(invoke('window_focus', { label: 'w-9' }))).toBe('NOT_FOUND');
    await invoke('window_focus', { label: 'w-2' });
    let list = await invoke<WindowInfo[]>('window_list');
    expect(list.filter((w) => w.focused).map((w) => w.label)).toEqual(['w-2']);
    await invoke('window_set_connection', { connId: 'c-local' });
    list = await invoke<WindowInfo[]>('window_list');
    expect(list[0]).toMatchObject({ label: 'main', connId: 'c-local', connName: 'local (SQLite)', title: 'BulkDB — local (SQLite)' });
    await invoke('window_set_connection', {});
    list = await invoke<WindowInfo[]>('window_list');
    expect(list[0]).toEqual({ label: 'main', index: 1, focused: false, title: 'BulkDB' });
  });

  it('allocates window labels monotonically, never reusing a closed window', () => {
    const labels = createWindowLabels('main'); // main counts as window 1
    expect(labels.next()).toBe('w-2');
    expect(labels.next()).toBe('w-3');
    // w-3 closes; observing the windows that remain must not lower the counter.
    labels.observe('main');
    labels.observe('w-2');
    expect(labels.next()).toBe('w-4'); // not w-3 again
    // A peer tab that already reached w-7 propagates its counter via a hello/state message.
    labels.raiseTo(7);
    expect(labels.next()).toBe('w-8');
    // A tab started as w-3 never hands out a label at or below its own.
    expect(createWindowLabels('w-3').next()).toBe('w-4');
  });

  it('instance_spawn simulates another BulkDB', async () => {
    expect((await invoke<InstancesInfo>('instances_info')).others).toEqual([]);
    await invoke('instance_spawn');
    const info = await invoke<InstancesInfo>('instances_info');
    expect(info.others.map((o) => [o.pid, o.kind])).toEqual([[2, 'desktop']]);
    mockBackend.reset();
    expect((await invoke<InstancesInfo>('instances_info')).others).toEqual([]);
  });
});
