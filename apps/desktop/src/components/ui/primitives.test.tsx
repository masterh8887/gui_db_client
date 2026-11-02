import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useRef, useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { askConfirm } from '@/lib/confirm';
import {
  Button,
  ConfirmHost,
  EnvTag,
  InspectorPanel,
  KeyHint,
  Popover,
  RowEstimate,
  SearchField,
  SegmentedControl,
  Spinner,
  StateChip,
  StatusBar,
  TabBar,
  TabItem,
  TooltipProvider,
  useListNavigation,
} from '@/components/ui';

afterEach(() => vi.restoreAllMocks());

describe('SegmentedControl', () => {
  const options = [
    { value: 'a', label: 'Alpha' },
    { value: 'b', label: 'Beta', disabled: true, title: 'not now' },
    { value: 'c', label: 'Gamma' },
  ];

  it('track variant: radiogroup; click selects and activates (the active option only activates)', async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onActivate = vi.fn();
    render(<SegmentedControl aria-label="seg" value="a" onChange={onChange} onActivate={onActivate} options={options} />);
    expect(screen.getByRole('radiogroup', { name: 'seg' })).toBeInTheDocument();
    expect(screen.getByRole('radio', { name: 'Alpha' })).toHaveAttribute('aria-checked', 'true');
    await user.click(screen.getByRole('radio', { name: 'Gamma' }));
    expect(onChange).toHaveBeenCalledWith('c');
    expect(onActivate).toHaveBeenCalledWith('c');
    await user.click(screen.getByRole('radio', { name: 'Alpha' }));
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onActivate).toHaveBeenLastCalledWith('a');
    await user.click(screen.getByRole('radio', { name: 'Beta' }));
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('arrow keys move the selection past disabled options; Enter / Space activate', async () => {
    const user = userEvent.setup();
    function Harness({ onActivate }: { onActivate: (v: string) => void }) {
      const [v, setV] = useState('a');
      return <SegmentedControl aria-label="seg" value={v} onChange={setV} onActivate={onActivate} options={options} />;
    }
    const onActivate = vi.fn();
    render(<Harness onActivate={onActivate} />);
    screen.getByRole('radio', { name: 'Alpha' }).focus();
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('radio', { name: 'Gamma' })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByRole('radio', { name: 'Gamma' })).toHaveFocus();
    expect(onActivate).not.toHaveBeenCalled();
    await user.keyboard('{Enter}');
    expect(onActivate).toHaveBeenCalledWith('c');
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('radio', { name: 'Alpha' })).toHaveAttribute('aria-checked', 'true');
    await user.keyboard(' ');
    expect(onActivate).toHaveBeenLastCalledWith('a');
  });

  it('tabs variant: tablist with aria-selected', () => {
    render(<SegmentedControl variant="tabs" aria-label="panel" value="c" onChange={() => undefined} options={options} />);
    expect(screen.getByRole('tablist', { name: 'panel' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Gamma' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tab', { name: 'Alpha' })).toHaveAttribute('aria-selected', 'false');
  });
});

describe('TabBar / TabItem', () => {
  function renderBar(over: Partial<Parameters<typeof TabItem>[0]> = {}) {
    const calls = { activate: vi.fn(), close: vi.fn(), reorder: vi.fn(), newTab: vi.fn(), rename: vi.fn(), cancel: vi.fn() };
    render(
      <TooltipProvider>
        <TabBar aria-label="tabs" onNewTab={calls.newTab} onReorder={calls.reorder}>
          <TabItem id="t1" title="쿼리 1" active onActivate={() => calls.activate('t1')} onClose={() => calls.close('t1')} {...over} />
          <TabItem id="t2" title="orders" active={false} dirty onActivate={() => calls.activate('t2')} onClose={() => calls.close('t2')} />
        </TabBar>
      </TooltipProvider>,
    );
    return calls;
  }

  it('activates on click, closes on middle-click and on its close button', async () => {
    const user = userEvent.setup();
    const calls = renderBar();
    expect(screen.getByRole('tablist', { name: 'tabs' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: /쿼리 1/ })).toHaveAttribute('aria-selected', 'true');
    await user.click(screen.getByRole('tab', { name: /orders/ }));
    expect(calls.activate).toHaveBeenCalledWith('t2');
    fireEvent(screen.getByRole('tab', { name: /orders/ }), new MouseEvent('auxclick', { bubbles: true, button: 1 }));
    expect(calls.close).toHaveBeenCalledWith('t2');
    await user.click(screen.getByRole('button', { name: '탭 닫기: 쿼리 1' }));
    expect(calls.close).toHaveBeenCalledWith('t1');
    expect(calls.activate).not.toHaveBeenCalledWith('t1');
    await user.click(screen.getByRole('button', { name: '새 탭' }));
    expect(calls.newTab).toHaveBeenCalled();
  });

  it('moves between tabs with the arrow keys (activating them)', async () => {
    const user = userEvent.setup();
    const calls = renderBar();
    screen.getByRole('tab', { name: /쿼리 1/ }).focus();
    await user.keyboard('{ArrowRight}');
    expect(calls.activate).toHaveBeenLastCalledWith('t2');
    expect(screen.getByRole('tab', { name: /orders/ })).toHaveFocus();
    await user.keyboard('{ArrowRight}');
    expect(calls.activate).toHaveBeenLastCalledWith('t1');
  });

  it('reorders by drag and drop', () => {
    const calls = renderBar();
    const data: Record<string, string> = {};
    const dataTransfer = {
      setData: (k: string, v: string) => (data[k] = v),
      getData: (k: string) => data[k] ?? '',
      get types() {
        return Object.keys(data);
      },
      effectAllowed: '',
      dropEffect: '',
    };
    fireEvent.dragStart(screen.getByRole('tab', { name: /orders/ }), { dataTransfer });
    fireEvent.dragOver(screen.getByRole('tab', { name: /쿼리 1/ }), { dataTransfer });
    fireEvent.drop(screen.getByRole('tab', { name: /쿼리 1/ }), { dataTransfer });
    expect(calls.reorder).toHaveBeenCalledWith(1, 0);
  });

  it('renames inline: Enter commits (empty = default), Esc cancels and is consumed', async () => {
    const user = userEvent.setup();
    const rename = vi.fn();
    const cancel = vi.fn();
    const { unmount } = render(
      <TooltipProvider>
        <TabBar aria-label="tabs" onNewTab={() => undefined}>
          <TabItem id="t1" title="쿼리 1" active renaming onRename={rename} onRenameCancel={cancel} onActivate={() => undefined} onClose={() => undefined} />
        </TabBar>
      </TooltipProvider>,
    );
    const input = screen.getByRole('textbox', { name: '쿼리 1' });
    await user.clear(input);
    await user.type(input, '일일 추출{Enter}');
    expect(rename).toHaveBeenCalledWith('일일 추출');
    unmount();

    const winKey = vi.fn();
    window.addEventListener('keydown', winKey);
    render(
      <TooltipProvider>
        <TabBar aria-label="tabs" onNewTab={() => undefined}>
          <TabItem id="t1" title="쿼리 1" active renaming onRename={rename} onRenameCancel={cancel} onActivate={() => undefined} onClose={() => undefined} />
        </TabBar>
      </TooltipProvider>,
    );
    await user.keyboard('{Escape}');
    expect(cancel).toHaveBeenCalled();
    expect(rename).toHaveBeenCalledTimes(1);
    expect(winKey.mock.calls.every(([e]) => (e as KeyboardEvent).key !== 'Escape' || (e as KeyboardEvent).defaultPrevented)).toBe(true);
    window.removeEventListener('keydown', winKey);
  });

  it('a rename started from the tab context menu takes focus once the menu closed; Esc then cancels', async () => {
    const user = userEvent.setup();
    const rename = vi.fn();
    const cancel = vi.fn();
    function Harness() {
      const [renaming, setRenaming] = useState(false);
      return (
        <TooltipProvider>
          <TabBar aria-label="tabs" onNewTab={() => undefined}>
            <TabItem
              id="t1"
              title="쿼리 1"
              active
              renaming={renaming}
              onRename={(t) => {
                rename(t);
                setRenaming(false);
              }}
              onRenameCancel={() => {
                cancel();
                setRenaming(false);
              }}
              contextItems={[{ id: 'rename', label: 'Rename', onSelect: () => setRenaming(true) }]}
              onActivate={() => undefined}
              onClose={() => undefined}
            />
          </TabBar>
        </TooltipProvider>
      );
    }
    render(<Harness />);
    fireEvent.contextMenu(screen.getByRole('tab', { name: /쿼리 1/ }));
    await user.click(await screen.findByRole('menuitem', { name: 'Rename' }));
    const input = await screen.findByRole('textbox', { name: '쿼리 1' });
    await waitFor(() => expect(input).toHaveFocus());
    expect(rename).not.toHaveBeenCalled();
    await user.keyboard('{Escape}');
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(rename).not.toHaveBeenCalled();
    expect(screen.queryByRole('textbox')).toBeNull();

    // Enter on the unchanged title cancels: the default title never becomes a custom one.
    fireEvent.contextMenu(screen.getByRole('tab', { name: /쿼리 1/ }));
    await user.click(await screen.findByRole('menuitem', { name: 'Rename' }));
    const again = await screen.findByRole('textbox', { name: '쿼리 1' });
    await waitFor(() => expect(again).toHaveFocus());
    await user.keyboard('{Enter}');
    expect(cancel).toHaveBeenCalledTimes(2);
    expect(rename).not.toHaveBeenCalled();
  });
});

describe('Popover', () => {
  function Harness({ initialFocus = true }: { initialFocus?: boolean }) {
    const anchor = useRef<HTMLButtonElement>(null);
    const [open, setOpen] = useState(false);
    return (
      <>
        <button ref={anchor} onClick={() => setOpen((o) => !o)}>
          anchor
        </button>
        <button>outside</button>
        <Popover open={open} onOpenChange={setOpen} anchor={anchor} aria-label="details" initialFocus={initialFocus}>
          <button>inside</button>
        </Popover>
        <div data-radix-popper-content-wrapper="">
          <button>menu item</button>
        </div>
      </>
    );
  }

  it('moves focus in, closes on Esc (consumed) and returns focus to the anchor', async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.click(screen.getByRole('button', { name: 'anchor' }));
    const dialog = screen.getByRole('dialog', { name: 'details' });
    expect(dialog).toHaveAttribute('aria-modal', 'false');
    expect(screen.getByRole('button', { name: 'inside' })).toHaveFocus();
    const winKey = vi.fn();
    window.addEventListener('keydown', winKey);
    await user.keyboard('{Escape}');
    window.removeEventListener('keydown', winKey);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(winKey).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'anchor' })).toHaveFocus();
  });

  it('closes on an outside pointerdown, but not inside a Radix popper or on its anchor', async () => {
    const user = userEvent.setup();
    render(<Harness initialFocus={false} />);
    await user.click(screen.getByRole('button', { name: 'anchor' }));
    expect(screen.getByRole('button', { name: 'anchor' })).toHaveFocus();
    fireEvent.pointerDown(screen.getByRole('button', { name: 'menu item' }));
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    fireEvent.pointerDown(screen.getByRole('button', { name: 'inside' }));
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    fireEvent.pointerDown(screen.getByRole('button', { name: 'outside' }));
    expect(screen.queryByRole('dialog')).toBeNull();
    // The anchor toggles it itself.
    await user.click(screen.getByRole('button', { name: 'anchor' }));
    await user.click(screen.getByRole('button', { name: 'anchor' }));
    expect(screen.queryByRole('dialog')).toBeNull();
  });
});

describe('KeyHint, RowEstimate, StateChip, EnvTag', () => {
  it('KeyHint renders the platform label', () => {
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    const { unmount } = render(<KeyHint combo="mod+shift+enter" />);
    expect(screen.getByText('Ctrl+Shift+Enter')).toBeInTheDocument();
    unmount();
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('MacIntel');
    render(<KeyHint combo="mod+shift+enter" />);
    expect(screen.getByText('⇧⌘↩')).toBeInTheDocument();
  });

  it('RowEstimate always says estimated (추정) or exact (정확) with the count time', () => {
    render(
      <TooltipProvider>
        <RowEstimate rows={BigInt(48_213_000)} variant="compact" />
        <RowEstimate rows={48_213_000} variant="compact" showLabel />
        <RowEstimate rows={48_213_000} variant="full" />
        <RowEstimate exact={{ table: { name: 't' }, rows: BigInt(48_213_004), elapsedMs: BigInt(12_300), at: '2026-10-27T04:02:11' }} variant="compact" />
        <RowEstimate variant="compact" />
      </TooltipProvider>,
    );
    expect(screen.getByText('≈48.2M')).toHaveAttribute('aria-label', '약 48,213,000행, 추정');
    expect(screen.getByText('≈48.2M 추정')).toBeInTheDocument();
    expect(screen.getByText('≈ 48,213,000 (추정)')).toBeInTheDocument();
    const exact = screen.getByText('48,213,004 · 10-27');
    expect(exact.getAttribute('aria-label')).toMatch(/^정확한 행 수 48,213,004/);
    expect(screen.getByText('—')).toHaveAttribute('aria-label', '행 수 추정 없음');
  });

  it('StateChip shows the state label with its icon; EnvTag shows the English key', () => {
    render(
      <>
        <StateChip state="done" />
        <StateChip state="running" />
        <EnvTag tag="production" variant="chip" />
        <EnvTag tag={undefined} variant="chip" />
      </>,
    );
    expect(screen.getByText('완료')).toBeInTheDocument();
    // The busy chip's spinner is decorative: the state label beside it is the text.
    expect(screen.queryByRole('status')).toBeNull();
    expect(document.querySelector('[data-state="running"] .animate-spin')).toHaveAttribute('aria-hidden', 'true');
    expect(screen.getByText('production')).toHaveAttribute('title', '운영');
  });

  it('Spinner is a localised status unless decorative; a loading Button is aria-busy with a hidden spinner', () => {
    render(
      <>
        <Spinner />
        <Spinner label="연결 확인 중" />
        <Spinner decorative />
        <Button loading>저장</Button>
      </>,
    );
    expect(screen.getAllByRole('status').map((s) => s.getAttribute('aria-label'))).toEqual(['불러오는 중…', '연결 확인 중']);
    const button = screen.getByRole('button', { name: '저장' });
    expect(button).toHaveAttribute('aria-busy', 'true');
    expect(button.querySelector('.animate-spin')).toHaveAttribute('aria-hidden', 'true');
  });

  it('filled buttons use the white-text fill tokens, never the text colours', () => {
    render(
      <>
        <Button variant="primary">실행</Button>
        <Button variant="dangerFilled">예</Button>
      </>,
    );
    const primary = screen.getByRole('button', { name: '실행' }).className;
    expect(primary).toContain('hover:bg-accent-fill-hover');
    expect(primary).not.toContain('hover:bg-accent-hover');
    const danger = screen.getByRole('button', { name: '예' }).className;
    expect(danger).toContain('bg-danger-fill');
    expect(danger).toContain('hover:bg-danger-fill-hover');
    expect(danger).not.toMatch(/(^|\s)bg-danger(\s|$)/);
    expect(danger).not.toContain('brightness-110');
  });

  it('StatusBar paints bg-panel, except inside a sidebar surface; InspectorPanel names itself in the UI language', () => {
    render(
      <>
        <StatusBar>main</StatusBar>
        <div data-surface="sidebar">
          <StatusBar surface="inherit">side</StatusBar>
        </div>
        <InspectorPanel segment="a" onSegmentChange={() => undefined} segments={[{ value: 'a', label: 'A' }]}>
          body
        </InspectorPanel>
      </>,
    );
    const main = screen.getByText('main').className;
    expect(main).toContain('bg-panel');
    expect(main).toContain('[[data-surface=sidebar]_&]:bg-transparent');
    const side = screen.getByText('side').className;
    expect(side).toContain('bg-transparent');
    expect(side).not.toContain('bg-panel');
    expect(screen.getByRole('tablist', { name: '인스펙터' }).closest('[data-surface="sidebar"]')).not.toBeNull();
  });
});

describe('SearchField', () => {
  function Harness() {
    const [v, setV] = useState('ord');
    return <SearchField aria-label="search" value={v} onChange={setV} shortcut="mod+shift+f" />;
  }

  it('Esc with text clears it and is consumed; Esc on an empty field passes through', async () => {
    const user = userEvent.setup();
    const winKey = vi.fn();
    window.addEventListener('keydown', winKey);
    render(<Harness />);
    const input = screen.getByRole('searchbox', { name: 'search' });
    await user.click(input);
    await user.keyboard('{Escape}');
    expect(input).toHaveValue('');
    expect(winKey).not.toHaveBeenCalled();
    await user.keyboard('{Escape}');
    expect(winKey).toHaveBeenCalledTimes(1);
    expect((winKey.mock.calls[0][0] as KeyboardEvent).defaultPrevented).toBe(false);
    window.removeEventListener('keydown', winKey);
  });

  it('clears with its button and shows the KeyHint while empty and unfocused', async () => {
    const user = userEvent.setup();
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    render(<Harness />);
    await user.click(screen.getByRole('button', { name: '검색어 지우기' }));
    expect(screen.getByRole('searchbox')).toHaveValue('');
    act(() => screen.getByRole('searchbox').blur());
    expect(screen.getByText('Ctrl+Shift+F')).toBeInTheDocument();
  });
});

describe('useListNavigation', () => {
  function List({ onOpen, onDelete }: { onOpen: (i: number) => void; onDelete: (i: number) => void }) {
    const items = ['a', 'b', 'c'];
    const [active, setActive] = useState(0);
    const nav = useListNavigation({ count: items.length, activeIndex: active, onMove: setActive, onOpen, onDelete });
    return (
      <div {...nav.listProps} aria-label="list">
        {items.map((it, i) => (
          <div key={it} {...nav.getItemProps(i)}>
            {it}
          </div>
        ))}
      </div>
    );
  }

  it('moves with arrows / Home / End, opens with Enter and deletes with Delete', async () => {
    const user = userEvent.setup();
    vi.spyOn(navigator, 'platform', 'get').mockReturnValue('Win32');
    const onOpen = vi.fn();
    const onDelete = vi.fn();
    render(<List onOpen={onOpen} onDelete={onDelete} />);
    const list = screen.getByRole('listbox', { name: 'list' });
    list.focus();
    await user.keyboard('{ArrowDown}');
    expect(screen.getByRole('option', { name: 'b' })).toHaveAttribute('aria-selected', 'true');
    expect(list.getAttribute('aria-activedescendant')).toBe(screen.getByRole('option', { name: 'b' }).id);
    await user.keyboard('{End}{Enter}');
    expect(onOpen).toHaveBeenCalledWith(2);
    await user.keyboard('{Home}{Delete}');
    expect(onDelete).toHaveBeenCalledWith(0);
    await user.keyboard('{Control>}{Backspace}{/Control}');
    expect(onDelete).toHaveBeenCalledTimes(2);
    await user.dblClick(screen.getByRole('option', { name: 'c' }));
    expect(onOpen).toHaveBeenLastCalledWith(2);
  });
});

describe('ConfirmHost', () => {
  it('resolves true on confirm and false on Esc', async () => {
    const user = userEvent.setup();
    render(<ConfirmHost />);
    let answer: Promise<boolean> = Promise.resolve(false);
    act(() => {
      answer = askConfirm({ title: '삭제할까요?', confirmLabel: '삭제', danger: true });
    });
    await user.click(await screen.findByRole('button', { name: '삭제' }));
    await expect(answer).resolves.toBe(true);
    act(() => {
      answer = askConfirm({ title: '다시?' });
    });
    await screen.findByRole('dialog', { name: '다시?' });
    await user.keyboard('{Escape}');
    await expect(answer).resolves.toBe(false);
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });
});
