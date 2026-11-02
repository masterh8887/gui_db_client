import { describe, expect, it } from 'vitest';
import { cn } from './utils';

describe('cn', () => {
  it('joins like clsx', () => {
    expect(cn('a', false, null, undefined, ['b', { c: true, d: false }])).toBe('a b c');
    expect(cn('flex')).toBe('flex');
  });

  it('a later utility of the same group replaces the earlier one (overrides passed as className win)', () => {
    expect(cn('flex h-(--h-bar) gap-2 px-2 text-sm', 'gap-0.5 px-1')).toBe('flex h-(--h-bar) text-sm gap-0.5 px-1');
    expect(cn('h-auto px-0 text-accent', 'h-(--h-control-sm) px-2 text-sm')).toBe('text-accent h-(--h-control-sm) px-2 text-sm');
    expect(cn('text-fg text-sm', 'text-fg-secondary')).toBe('text-sm text-fg-secondary');
    expect(cn('inline-flex items-center', 'hidden')).toBe('items-center hidden');
    expect(cn('rounded-md bg-btn', 'rounded-[4px] bg-transparent')).toBe('rounded-[4px] bg-transparent');
  });

  it('keeps different groups, variants and arbitrary values apart', () => {
    expect(cn('px-2 py-1 pr-1')).toBe('px-2 py-1 pr-1');
    expect(cn('bg-window hover:bg-hover aria-selected:bg-selected', 'hover:bg-pressed')).toBe('bg-window aria-selected:bg-selected hover:bg-pressed');
    expect(cn('text-sm font-mono font-medium text-left')).toBe('text-sm font-mono font-medium text-left');
    expect(cn('[&_svg]:size-3.5 min-w-0 max-w-[55%] w-full', 'mx-auto -ml-1')).toBe('[&_svg]:size-3.5 min-w-0 max-w-[55%] w-full mx-auto -ml-1');
    expect(cn('supports-[display:grid]:grid flex')).toBe('supports-[display:grid]:grid flex');
  });
});
