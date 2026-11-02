'use client';

// Welcome's activity list (DESIGN.md §5.F "JobChip"): a Popover anchored to the JobChip, opened
// by `showActivity()` (ui.activityPopoverOpen). It never takes focus; closing it also drops a
// pending cancel confirmation. It lives inside JobChip, which unmounts when the jobs store empties
// ([지우기]) or when Welcome gives way to a workspace; unmounting closes it too, so the flag never
// stays set without a popover on screen (it would pop open by itself with the next job, and
// `showActivity()` would take it as already open).

import { useEffect, type RefObject } from 'react';
import { Popover } from '@/components/ui';
import { useUiStore } from '@/stores/ui';
import { ActivityPanel } from './ActivityPanel';
import { ms } from './strings';

export function closeActivityPopover(): void {
  const ui = useUiStore.getState();
  ui.setActivityPopoverOpen(false);
  if (ui.cancelConfirmJobId) ui.setCancelConfirm(null);
}

export function ActivityPopover({ anchor }: { anchor: RefObject<HTMLElement | null> }) {
  const open = useUiStore((s) => s.activityPopoverOpen);
  useEffect(
    () => () => {
      if (useUiStore.getState().activityPopoverOpen) closeActivityPopover();
    },
    [],
  );
  return (
    <Popover
      open={open}
      onOpenChange={(o) => (o ? useUiStore.getState().setActivityPopoverOpen(true) : closeActivityPopover())}
      anchor={anchor}
      align="end"
      width={360}
      initialFocus={false}
      aria-label={ms('popover')}
    >
      <ActivityPanel variant="popover" />
    </Popover>
  );
}
