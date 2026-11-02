'use client';

// Console drawer frame (DESIGN.md §5.G): full width at the window bottom in both modes,
// `bg-panel`, top hairline, resizable (`ui.logHeight`). Closing it (X, Esc) acknowledges the
// error flag: the user has seen what auto-opened it.

import { ConsoleDrawer } from '@/components/log/ConsoleDrawer';
import { useLogsStore } from '@/stores/logs';
import { useUiStore } from '@/stores/ui';
import { ResizeHandle } from './ResizeHandle';
import { ls } from './strings';

export function ConsoleFrame() {
  const height = useUiStore((s) => s.logHeight);
  const setHeight = useUiStore((s) => s.setLogHeight);
  const setOpen = useUiStore((s) => s.setLogOpen);
  return (
    <section aria-label={ls('console')} className="relative shrink-0 border-t border-separator bg-panel" style={{ height }}>
      <ResizeHandle edge="top" size={height} onResize={setHeight} label={ls('resizeConsole')} />
      <ConsoleDrawer
        onClose={() => {
          useLogsStore.getState().ackError();
          setOpen(false);
        }}
      />
    </section>
  );
}
