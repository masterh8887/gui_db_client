'use client';

// Footer "최근 복사…" (docs/db-copy.md 7): a menu of `copy_runs` rows with 이어서 실행 / 결과 보기 /
// 기록에서 삭제. 이어서 실행 always opens the run's result first (`showRun`), whose [이어서 실행]
// asks what a resume needs: the partial policy when tables were appended into, the production
// name, and a missing "ask every time" password. It never resumes straight from the menu, so no
// acknowledgement of another plan the wizard holds is carried into the run.

import type { CopyRunInfo } from 'ipc-types';
import { History } from 'lucide-react';
import { useEffect, useState } from 'react';
import { Button, DropdownMenu, type MenuItem } from '@/components/ui';
import { copyForget, copyRuns } from '@/lib/commands';
import { askConfirm } from '@/lib/confirm';
import { stateLabel } from '@/lib/i18n';
import { errorText } from '@/lib/ipc-error';
import { formatDateTime } from '@/lib/format';
import { toast } from '@/lib/toast';
import { useCopyStore } from '@/stores/copy';
import { cs } from './strings';

async function forget(run: CopyRunInfo, reload: () => void): Promise<void> {
  if (!(await askConfirm({ title: cs('forgetTitle'), body: cs('forgetBody'), confirmLabel: cs('runForget'), danger: true }))) return;
  try {
    await copyForget(run.runId);
    reload();
  } catch (e) {
    toast(errorText(e), { tone: 'danger' });
  }
}

export function RecentRuns({ disabled }: { disabled?: boolean }) {
  const [open, setOpen] = useState(false);
  const [runs, setRuns] = useState<CopyRunInfo[] | null>(null);
  const [gen, setGen] = useState(0);
  useEffect(() => {
    if (!open) return;
    let alive = true;
    copyRuns(undefined, 10)
      .then((list) => alive && setRuns(list))
      .catch(() => alive && setRuns([]));
    return () => {
      alive = false;
    };
  }, [open, gen]);

  const reload = () => {
    setRuns(null);
    setGen((g) => g + 1);
  };
  const onOpenChange = (o: boolean) => {
    if (o) setRuns(null);
    setOpen(o);
  };

  const items: MenuItem[] =
    runs === null
      ? [{ id: 'loading', kind: 'label', label: cs('recentLoading') }]
      : runs.length === 0
        ? [{ id: 'empty', kind: 'label', label: cs('recentEmpty') }]
        : runs.flatMap((r, i) => [
            {
              id: `${r.runId}:label`,
              kind: 'label' as const,
              separatorBefore: i > 0,
              label: cs('runLine', { label: r.label, state: stateLabel(r.state), time: formatDateTime(r.startedAt) }),
            },
            { id: `${r.runId}:resume`, label: cs('runResume'), disabled: !r.resumable || r.inUse, onSelect: () => void useCopyStore.getState().showRun(r.runId) },
            { id: `${r.runId}:show`, label: cs('runShow'), onSelect: () => void useCopyStore.getState().showRun(r.runId) },
            { id: `${r.runId}:forget`, label: cs('runForget'), danger: true, disabled: r.inUse, onSelect: () => void forget(r, reload) },
          ]);

  return (
    <DropdownMenu items={items} align="start" side="top" open={open} onOpenChange={onOpenChange}>
      <Button variant="ghost" disabled={disabled} icon={<History size={14} strokeWidth={1.75} />}>
        {cs('recent')}
      </Button>
    </DropdownMenu>
  );
}
