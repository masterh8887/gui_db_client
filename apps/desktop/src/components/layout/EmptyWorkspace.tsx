'use client';

// Shown in the tab content area when the workspace has no tabs.

import { runAction } from '@/lib/actions';
import { KeyHint } from '@/components/ui';
import { ls } from './strings';

const ROWS = [
  { action: 'tabs.newQuery', combo: 'mod+t', label: () => ls('emptyNewQuery') },
  { action: 'palette.open', combo: 'mod+p', label: () => ls('emptyPalette') },
  { action: 'sidebar.focusSearch', combo: 'mod+shift+f', label: () => ls('emptySearch') },
] as const;

export function EmptyWorkspace() {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 text-fg-secondary">
      <div className="text-base">{ls('emptyTitle')}</div>
      <div className="flex flex-col gap-1">
        {ROWS.map((r) => (
          <button
            key={r.action}
            type="button"
            onClick={() => void runAction(r.action)}
            className="flex h-(--h-row) w-60 items-center justify-between gap-6 rounded-md px-2 text-sm hover:bg-hover hover:text-fg"
          >
            <span>{r.label()}</span>
            <KeyHint combo={r.combo} plain />
          </button>
        ))}
      </div>
    </div>
  );
}
