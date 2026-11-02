'use client';

// Context menu (and hover `⋯` menu) of a sidebar row (DESIGN.md §5.C "Sidebar"): 열기 (새 탭),
// 구조 보기, 추출 쿼리 만들기, 정확한 행 수 세기 | 열 목록 복사, 이름 복사 | 새로 고침.

import type { TableRef } from 'ipc-types';
import { Calculator, Columns3, Copy, FileCode2, RotateCw, SquarePlus, TextCursorInput } from 'lucide-react';
import type { MenuItem } from '@/components/ui';
import {
  canTargetTable,
  copyColumnList,
  copyTableName,
  countExactly,
  openExtractionQuery,
} from '@/lib/table-actions';
import { useTabsStore } from '@/stores/tabs';
import { ts } from './strings';

/** Open `ref` as a pinned table tab showing 구조 (Enter, double-click, 구조 보기). */
export function openStructure(connId: string, ref: TableRef): string {
  return useTabsStore.getState().openTable(connId, ref, { preview: false, view: 'structure' });
}

export function tableMenuItems(opts: {
  connId: string;
  ref: TableRef;
  /** An exact count of this table is running. */
  counting: boolean;
  onRefresh: () => void;
}): MenuItem[] {
  const { connId, ref, counting, onRefresh } = opts;
  const target = canTargetTable(connId, ref);
  const icon = (I: typeof Copy) => <I size={14} strokeWidth={1.75} />;
  return [
    {
      id: 'open-new-tab',
      label: ts('menuOpenNewTab'),
      icon: icon(SquarePlus),
      onSelect: () => void useTabsStore.getState().openTable(connId, ref, { newTab: true }),
    },
    {
      id: 'structure',
      label: ts('menuStructure'),
      icon: icon(Columns3),
      shortcut: 'enter',
      onSelect: () => void openStructure(connId, ref),
    },
    {
      id: 'make-query',
      label: ts('menuMakeQuery'),
      icon: icon(FileCode2),
      shortcut: 'mod+enter',
      disabled: !target.ok,
      title: target.ok ? undefined : target.reason,
      onSelect: () => void openExtractionQuery(connId, ref),
    },
    {
      id: 'count',
      label: counting ? ts('menuCounting') : ts('menuCount'),
      icon: icon(Calculator),
      disabled: counting,
      onSelect: () => void countExactly(connId, ref),
    },
    {
      id: 'copy-columns',
      label: ts('menuCopyColumns'),
      icon: icon(Copy),
      separatorBefore: true,
      onSelect: () => void copyColumnList(connId, ref),
    },
    {
      id: 'copy-name',
      label: ts('menuCopyName'),
      icon: icon(TextCursorInput),
      onSelect: () => void copyTableName(ref),
    },
    {
      id: 'refresh',
      label: ts('menuRefresh'),
      icon: icon(RotateCw),
      separatorBefore: true,
      onSelect: onRefresh,
    },
  ];
}
