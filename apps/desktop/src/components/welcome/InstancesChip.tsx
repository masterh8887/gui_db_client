'use client';

// "다른 BulkDB {n}개 실행 중" (docs/db-copy.md 1.2): shown in Welcome's header and the workspace
// status pill popover while other live BulkDB processes share this config directory. The
// tooltip says what is per process and what is shared.

import { Badge, Tooltip } from '@/components/ui';
import { otherInstanceCount, useWindowsStore, wins } from '@/stores/windows';
import { ws } from './strings';

export function InstancesChip({ className }: { className?: string }) {
  const n = useWindowsStore((s) => otherInstanceCount(s));
  if (n === 0) return null;
  return (
    <Tooltip content={wins('otherInstancesTip')}>
      <Badge tone="neutral" hangul data-instances-chip="" tabIndex={0} className={className}>
        {ws('otherInstances', { n })}
      </Badge>
    </Tooltip>
  );
}
