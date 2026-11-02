'use client';

import * as RadixSelect from '@radix-ui/react-select';
import { Check, ChevronsUpDown } from 'lucide-react';
import { ENV_TAGS, envLabel, envOptionLabel, isEnvTag, type EnvTag as EnvTagValue } from '@/lib/env';
import { cn } from '@/lib/utils';
import { selectContentClass, selectItemClass } from './Select';
import { us } from './strings';

export interface EnvTagProps {
  tag: EnvTagValue | undefined;
  /**
   * dot: 8px circle; text: lowercase key in the tag colour (11px); chip: filled pill with the
   * uppercase key (status pill); badge: filled small rounded key. chip / badge keep their fill
   * inside a focused selection.
   */
  variant: 'dot' | 'text' | 'chip' | 'badge';
  className?: string;
}

/** Environment tag mark. Production is red wherever it appears (DESIGN.md §2.7). */
export function EnvTag({ tag, variant, className }: EnvTagProps) {
  if (!tag) {
    if (variant !== 'dot') return null;
    return <span aria-hidden className={cn('inline-block h-2 w-2 shrink-0 rounded-full shadow-[inset_0_0_0_1px_var(--fg-tertiary)]', className)} />;
  }
  const label = envLabel(tag);
  switch (variant) {
    case 'dot':
      return (
        <span
          role="img"
          aria-label={label}
          title={label}
          className={cn('inline-block h-2 w-2 shrink-0 rounded-full', className)}
          style={{ background: `var(--env-${tag})` }}
        />
      );
    case 'text':
      return (
        <span className={cn('sel-muted shrink-0 text-xs font-medium', className)} style={{ color: `var(--env-${tag})` }} title={label}>
          {tag}
        </span>
      );
    case 'chip':
      return (
        <span
          title={label}
          className={cn('inline-flex h-[18px] shrink-0 items-center rounded-full px-2 text-xs font-semibold uppercase tracking-wide text-white', className)}
          style={{ background: `var(--env-${tag}-fill)` }}
        >
          {tag}
        </span>
      );
    case 'badge':
      return (
        <span
          title={label}
          className={cn('inline-flex h-4 shrink-0 items-center rounded-sm px-1.5 text-xs font-medium text-white', className)}
          style={{ background: `var(--env-${tag}-fill)` }}
        >
          {tag}
        </span>
      );
  }
}

const NONE = 'none';

export interface TagPickerProps {
  value: EnvTagValue | undefined;
  onChange: (t: EnvTagValue | undefined) => void;
  id?: string;
  className?: string;
}

/** Tag select: "태그 없음" + the five tags with their dot and `production — 운영`. */
export function TagPicker({ value, onChange, id, className }: TagPickerProps) {
  return (
    <RadixSelect.Root value={value ?? NONE} onValueChange={(v) => onChange(isEnvTag(v) ? v : undefined)}>
      <RadixSelect.Trigger
        id={id}
        aria-label={us('tagLabel')}
        className={cn(
          'input-focus inline-flex h-(--h-control) min-w-0 items-center justify-between gap-1.5 rounded-md border border-border-input bg-input px-2 text-left text-base text-fg',
          className,
        )}
      >
        <span className="flex min-w-0 items-center gap-1.5">
          <EnvTag tag={value} variant="dot" />
          <span className="truncate">{value ? envOptionLabel(value) : us('tagNone')}</span>
        </span>
        <ChevronsUpDown size={12} strokeWidth={1.75} className="shrink-0 text-fg-secondary" />
      </RadixSelect.Trigger>
      <RadixSelect.Portal>
        <RadixSelect.Content position="popper" sideOffset={4} className={selectContentClass}>
          <RadixSelect.Viewport>
            <RadixSelect.Item value={NONE} className={selectItemClass}>
              <RadixSelect.ItemIndicator className="absolute left-1.5 inline-flex items-center">
                <Check size={12} strokeWidth={2} />
              </RadixSelect.ItemIndicator>
              <span className="mr-1.5 inline-block h-2 w-2 rounded-full shadow-[inset_0_0_0_1px_var(--fg-tertiary)]" />
              <RadixSelect.ItemText>{us('tagNone')}</RadixSelect.ItemText>
            </RadixSelect.Item>
            {ENV_TAGS.map((tag) => (
              <RadixSelect.Item key={tag} value={tag} className={selectItemClass}>
                <RadixSelect.ItemIndicator className="absolute left-1.5 inline-flex items-center">
                  <Check size={12} strokeWidth={2} />
                </RadixSelect.ItemIndicator>
                <span className="mr-1.5 inline-block h-2 w-2 rounded-full" style={{ background: `var(--env-${tag})` }} />
                <RadixSelect.ItemText>{envOptionLabel(tag)}</RadixSelect.ItemText>
              </RadixSelect.Item>
            ))}
          </RadixSelect.Viewport>
        </RadixSelect.Content>
      </RadixSelect.Portal>
    </RadixSelect.Root>
  );
}
