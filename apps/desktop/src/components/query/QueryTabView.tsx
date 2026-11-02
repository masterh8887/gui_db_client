'use client';

// Query tab (DESIGN.md §5.D): the CodeMirror editor fills the tab, the 28px query bar sits under
// it and the export panel (collapsed for new docs) under the bar. Everything works on this tab's
// document (`extract.docs[tabId]`); the store does the work, this view renders state and
// forwards clicks and keys. The tab stays mounted while hidden, so the editor keeps its undo
// history, selection and scroll.

import type { JobState } from 'ipc-types';
import { FileText, FolderOpen } from 'lucide-react';
import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent as ReactPointerEvent, type RefObject } from 'react';
import { pickSqlFile } from '@/lib/dialogs';
import { formatBytes, formatDateTime } from '@/lib/format';
import { cn } from '@/lib/utils';
import { Button, Input, Notice } from '@/components/ui';
import { useExtractStore, useQueryDoc } from '@/stores/extract';
import { useJobsStore } from '@/stores/jobs';
import { useTabsStore } from '@/stores/tabs';
import { LAYOUT_LIMITS, useUiStore } from '@/stores/ui';
import { ExportPanel } from './ExportPanel';
import { PanelEmpty } from './parts';
import { isStoppable, QueryBar } from './QueryBar';
import { SqlEditor } from './SqlEditor';
import { qs } from './strings';

/** Whether `tabId` is the active tab of its workspace (hidden tabs stay mounted). */
function useTabVisible(tabId: string): boolean {
  return useTabsStore((s) => {
    const tab = s.tabs[tabId];
    return !!tab && s.sets[tab.connId]?.activeId === tabId;
  });
}

/** Clamp an export panel height to the ui limits and half of the tab. */
export function clampPanelHeight(px: number, tabHeight: number): number {
  const upper = Math.max(LAYOUT_LIMITS.exportMin, Math.min(LAYOUT_LIMITS.exportMax, tabHeight > 0 ? tabHeight * 0.5 : LAYOUT_LIMITS.exportMax));
  return Math.round(Math.max(LAYOUT_LIMITS.exportMin, Math.min(upper, px)));
}

/** The splitter between the editor and the query bar (drag, or ↑↓ with focus). */
function PanelResizer({ rootRef, height }: { rootRef: RefObject<HTMLDivElement | null>; height: number }) {
  const [dragging, setDragging] = useState(false);
  const resize = (px: number) => useUiStore.getState().setExportPanelHeight(clampPanelHeight(px, rootRef.current?.clientHeight ?? 0));

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const startY = e.clientY;
    const start = height;
    setDragging(true);
    const move = (ev: PointerEvent) => resize(start + (startY - ev.clientY));
    const up = () => {
      setDragging(false);
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 64 : 16;
    if (e.key === 'ArrowUp') resize(height + step);
    else if (e.key === 'ArrowDown') resize(height - step);
    else return;
    e.preventDefault();
  };

  return (
    <div className="relative z-10 h-0 shrink-0">
      <div
        role="separator"
        aria-orientation="horizontal"
        aria-label={qs('resize')}
        aria-valuenow={height}
        aria-valuemin={LAYOUT_LIMITS.exportMin}
        aria-valuemax={LAYOUT_LIMITS.exportMax}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onKeyDown={onKeyDown}
        className={cn(
          'absolute inset-x-0 -top-[3px] h-[6px] cursor-row-resize outline-none focus-visible:bg-focus',
          dragging && 'bg-focus',
        )}
      />
    </div>
  );
}

/** File mode: path + [찾아보기…] + [불러오기], size · modified, then the read-only contents. */
function SqlFilePane({ tabId, visible, onRun, onExplain, onCheck }: { tabId: string; visible: boolean; onRun: () => void; onExplain: () => void; onCheck: () => void }) {
  const doc = useQueryDoc(tabId, (d) => ({ connId: d.connId, sqlFile: d.sqlFile, sqlFileInfo: d.sqlFileInfo, busy: d.busy }));
  if (!doc) return null;
  const ex = useExtractStore.getState;
  const browse = async () => {
    const path = await pickSqlFile();
    if (path) await ex().loadSqlFile(tabId, path);
  };
  const info = doc.sqlFileInfo;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-(--h-bar) shrink-0 items-center gap-1.5 border-b border-separator px-2">
        <FileText size={14} strokeWidth={1.75} className="shrink-0 text-fg-secondary" aria-hidden />
        <Input
          dense
          aria-label={qs('filePath')}
          value={doc.sqlFile}
          placeholder={qs('filePlaceholder')}
          onChange={(e) => ex().setSqlFile(tabId, e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && doc.sqlFile.trim()) {
              e.preventDefault();
              void ex().loadSqlFile(tabId);
            }
          }}
          className="max-w-[640px]"
        />
        <Button size="sm" variant="secondary" icon={<FolderOpen size={14} strokeWidth={1.75} />} onClick={() => void browse()}>
          {qs('browse')}
        </Button>
        <Button size="sm" variant="secondary" loading={doc.busy === 'file'} disabled={!doc.sqlFile.trim()} onClick={() => void ex().loadSqlFile(tabId)}>
          {qs('loadFile')}
        </Button>
        {info && (
          <span className="ml-auto shrink-0 whitespace-nowrap text-sm tabular-nums text-fg-secondary">
            {formatBytes(info.sizeBytes)} · {formatDateTime(info.modifiedAt, true)} · {qs('readOnly')}
          </span>
        )}
      </div>
      <div className="min-h-0 flex-1">
        {info ? (
          <SqlEditor
            value={info.content}
            onChange={() => undefined}
            readOnly
            connId={doc.connId}
            visible={visible}
            onRun={onRun}
            onExplain={onExplain}
            onCheck={onCheck}
            aria-label={qs('fileContent')}
          />
        ) : (
          <PanelEmpty title={qs('fileHint')} />
        )}
      </div>
    </div>
  );
}

export function QueryTabView({ tabId }: { tabId: string }) {
  const rootRef = useRef<HTMLDivElement>(null);
  const visible = useTabVisible(tabId);
  const doc = useQueryDoc(tabId, (d) => ({
    connId: d.connId,
    sqlMode: d.sqlMode,
    sql: d.sql,
    busy: d.busy,
    error: d.error,
    panelCollapsed: d.panelCollapsed,
    lastJobId: d.lastJobId,
  }));
  const jobState = useJobsStore((s): JobState | undefined => (doc?.lastJobId ? s.jobs[doc.lastJobId]?.summary.state : undefined));
  const panelHeight = useUiStore((s) => s.exportPanelHeight);
  // Focus the editor when the tab is created on screen (a new tab, a table tab switched to 쿼리).
  const [autoFocus] = useState(visible);

  // The panel opens on 작업 when this tab's job starts waiting for acknowledgement.
  const prevState = useRef(jobState);
  useEffect(() => {
    if (jobState === 'warned' && prevState.current !== 'warned') useExtractStore.getState().setPanel(tabId, 'job');
    prevState.current = jobState;
  }, [jobState, tabId]);

  if (!doc) return <PanelEmpty title={qs('docMissing')} />;

  const ex = useExtractStore.getState;
  // The editor's keys act on this tab. While its job runs the bar offers 중지, not 실행, so
  // Mod-Enter does not start a second export from here.
  const onRun = () => {
    if (!isStoppable(jobState)) void ex().run(tabId);
  };
  const onExplain = () => void ex().explain(tabId);
  const onCheck = () => void ex().check(tabId);
  const expanded = !doc.panelCollapsed;

  return (
    <div ref={rootRef} className="@container flex h-full min-h-0 flex-col bg-window" data-tab-id={tabId}>
      <div className="relative min-h-0 flex-1">
        <div className={cn('absolute inset-0', doc.sqlMode !== 'text' && 'hidden')}>
          <SqlEditor
            value={doc.sql}
            onChange={(v) => ex().setSql(tabId, v)}
            connId={doc.connId}
            readOnly={doc.busy === 'select'}
            visible={visible && doc.sqlMode === 'text'}
            autoFocus={autoFocus && doc.sqlMode === 'text'}
            onRun={onRun}
            onExplain={onExplain}
            onCheck={onCheck}
            placeholder={qs('sqlPlaceholder')}
            aria-label={qs('sql')}
          />
        </div>
        {doc.sqlMode === 'file' && (
          <div className="absolute inset-0">
            <SqlFilePane tabId={tabId} visible={visible} onRun={onRun} onExplain={onExplain} onCheck={onCheck} />
          </div>
        )}
      </div>
      {expanded && <PanelResizer rootRef={rootRef} height={panelHeight} />}
      <QueryBar tabId={tabId} />
      {doc.error && (
        <div className="shrink-0 bg-panel px-2 pb-1.5">
          <Notice tone="danger" onClose={() => ex().clearError(tabId)}>
            <span className="block truncate" title={doc.error}>
              {doc.error}
            </span>
          </Notice>
        </div>
      )}
      {expanded && <ExportPanel tabId={tabId} height={panelHeight} />}
    </div>
  );
}
