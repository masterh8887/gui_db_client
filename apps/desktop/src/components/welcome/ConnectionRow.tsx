'use client';

// One 44px two-line row of the Welcome list (DESIGN.md §5.A) and its menu (context menu and the
// hover-only `⋯` button share the items).

import type { Connection, DriverInfo, WindowInfo } from 'ipc-types';
import { Ellipsis, PenLine } from 'lucide-react';
import type { CSSProperties } from 'react';
import {
  clearWriteAllowed,
  duplicateConnection,
  editConnection,
  newConnection,
  rescanDrivers,
  startImportDbeaver,
  testConnection,
} from '@/lib/connection-actions';
import { connTag } from '@/lib/env';
import { errorText } from '@/lib/ipc-error';
import { toast } from '@/lib/toast';
import { familyOf } from '@/components/copy/model';
import {
  Badge,
  ContextMenu,
  DropdownMenu,
  Highlight,
  IconButton,
  StatusDot,
  Tooltip,
  type ListNavigation,
  type MenuItem,
} from '@/components/ui';
import { useConnectionsStore, type ConnStatus } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useWindowsStore, wins } from '@/stores/windows';
import { useWorkspaceStore } from '@/stores/workspace';
import { connSubtitle } from './connection-list';
import { DriverBadge } from './DriverBadge';
import { MiddleTruncate } from './MiddleTruncate';
import { ws } from './strings';

/** Open `conn` in a new window (its workspace, password prompt there). */
export function openInNewWindow(connId: string): void {
  void useWindowsStore.getState().openNew(connId);
}

/** The copy wizard with `conn` as the source (its database preset) or as the target. */
export function openCopyWith(conn: Connection, side: 'source' | 'target'): void {
  const endpoint = side === 'source' ? { connId: conn.id, ...(conn.database ? { database: conn.database } : {}) } : { connId: conn.id };
  useCopyStore.getState().open(side === 'source' ? { source: endpoint } : { target: endpoint });
}

/**
 * Menu of a connection: 연결, 새 창에서 열기, 연결 테스트, 편집…, 복제 | 이 연결을 원본으로
 * 복사…, 이 연결을 대상으로 복사…, 쓰기 허용 해제 (write-allowed only) | 작업공간 닫기 (when
 * open) | 새 연결…, DBeaver에서 가져오기…, 드라이버 다시 검색 | 삭제.
 */
export function connectionMenuItems(
  conn: Connection,
  opts: { isOpen: boolean; mac: boolean; onDelete: () => void },
): MenuItem[] {
  const items: MenuItem[] = [
    { id: 'connect', label: ws('menuConnect'), shortcut: 'enter', onSelect: () => void useWorkspaceStore.getState().openWorkspace(conn.id) },
    { id: 'openInNewWindow', label: ws('menuOpenInNewWindow'), shortcut: 'mod+enter', onSelect: () => openInNewWindow(conn.id) },
    { id: 'test', label: ws('menuTest'), onSelect: () => void testConnection(conn.id) },
    { id: 'edit', label: ws('menuEdit'), onSelect: () => editConnection(conn.id) },
    {
      id: 'duplicate',
      label: ws('menuDuplicate'),
      onSelect: () =>
        void duplicateConnection(conn.id).then((copy) => {
          if (copy) useConnectionsStore.getState().select(copy.id);
        }),
    },
  ];
  // DB copy only supports postgres/mysql/sqlite. For mssql/oracle/odbc the wizard would open on a
  // "복사 지원 안 함" selection it cannot leave, so do not offer the copy entry points at all.
  if (familyOf(conn.driver)) {
    items.push(
      { id: 'copyAsSource', label: ws('menuCopyAsSource'), separatorBefore: true, onSelect: () => openCopyWith(conn, 'source') },
      { id: 'copyAsTarget', label: ws('menuCopyAsTarget'), onSelect: () => openCopyWith(conn, 'target') },
    );
  }
  if (conn.writeAllowed) {
    items.push({
      id: 'clearWriteAllowed',
      label: ws('menuClearWriteAllowed'),
      // The row menu has no dialog to surface a failed conn_save, so toast its rejection.
      onSelect: () => void clearWriteAllowed(conn).catch((e: unknown) => toast(errorText(e), { tone: 'danger' })),
    });
  }
  if (opts.isOpen) {
    items.push({
      id: 'closeWorkspace',
      label: ws('menuCloseWorkspace'),
      separatorBefore: true,
      onSelect: () => void useWorkspaceStore.getState().closeWorkspace(conn.id),
    });
  }
  items.push(
    { id: 'new', label: ws('menuNew'), shortcut: 'mod+n', separatorBefore: true, onSelect: newConnection },
    { id: 'import', label: ws('menuImport'), onSelect: startImportDbeaver },
    { id: 'rescan', label: ws('menuRescan'), onSelect: () => void rescanDrivers() },
    {
      id: 'delete',
      label: ws('menuDelete'),
      danger: true,
      separatorBefore: true,
      shortcut: opts.mac ? 'mod+backspace' : 'delete',
      onSelect: opts.onDelete,
    },
  );
  return items;
}

export interface ConnectionRowProps {
  conn: Connection;
  driver: DriverInfo | undefined;
  /** Drivers loaded (so a missing one is really missing). */
  driversKnown: boolean;
  nameIndices: readonly number[];
  /** Tabs of its in-memory workspace; `null` when no workspace is open for it. */
  tabCount: number | null;
  /** Other windows of this process showing this connection ("창 N" chips). */
  otherWindows?: readonly WindowInfo[];
  status: ConnStatus | undefined;
  error: string | undefined;
  menuItems: MenuItem[];
  itemProps: ReturnType<ListNavigation['getItemProps']>;
  onContextMenu: () => void;
}

export function ConnectionRow({
  conn,
  driver,
  driversKnown,
  nameIndices,
  tabCount,
  otherWindows = [],
  status,
  error,
  menuItems,
  itemProps,
  onContextMenu,
}: ConnectionRowProps) {
  const tag = connTag(conn);
  const sub = connSubtitle(conn);
  const driverMissing = driversKnown && (!driver || !driver.available);
  const tested = status !== undefined && status !== 'unknown';
  return (
    <ContextMenu items={menuItems}>
      <div
        {...itemProps}
        data-conn-id={conn.id}
        onContextMenu={onContextMenu}
        className="group flex h-(--h-row-2) items-center gap-2.5 rounded-md px-2 aria-selected:bg-selected"
      >
        <DriverBadge driverId={conn.driver} name={driver?.name} tag={tag} />
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <Highlight text={conn.name} indices={nameIndices} className="truncate font-medium" />
            {tag && (
              <span
                className="sel-muted shrink-0 text-xs font-medium text-(--tag-color)"
                style={{ '--tag-color': `var(--env-${tag})` } as CSSProperties}
              >
                {tag}
              </span>
            )}
            {conn.writeAllowed && (
              <Tooltip content={ws('writeAllowedTip')}>
                <span
                  data-write-allowed=""
                  role="img"
                  aria-label={ws('writeAllowedTip')}
                  className="sel-muted inline-flex shrink-0 self-center text-fg-secondary"
                >
                  <PenLine size={12} strokeWidth={1.75} />
                </span>
              </Tooltip>
            )}
          </div>
          {sub.text &&
            (sub.path ? (
              <MiddleTruncate text={sub.text} className="sel-muted text-sm text-fg-secondary" />
            ) : (
              <div className="sel-muted truncate text-sm text-fg-secondary" title={sub.text}>
                {sub.text}
              </div>
            ))}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {tested && <StatusDot status={status} title={status === 'error' ? error : undefined} />}
          {tabCount !== null && (
            <span className="sel-muted text-sm text-fg-secondary tabular-nums">
              {tabCount === 1 ? ws('openTab') : ws('openTabs', { n: tabCount })}
            </span>
          )}
          {otherWindows.map((w) => (
            <Tooltip key={w.label} content={wins('windowChipTip', { index: w.index, title: w.title })}>
              <button
                type="button"
                data-window-chip={w.label}
                onClick={(e) => {
                  e.stopPropagation();
                  void useWindowsStore.getState().focus(w.label);
                }}
                onDoubleClick={(e) => e.stopPropagation()}
                className="sel-muted inline-flex h-[18px] shrink-0 items-center rounded-sm bg-control px-1.5 text-sm font-medium leading-none text-fg-secondary tabular-nums hover:bg-hover hover:text-fg focus-visible:outline-2 focus-visible:outline-focus"
              >
                {ws('windowChip', { index: w.index })}
              </button>
            </Tooltip>
          ))}
          {driverMissing && (
            <Tooltip content={ws('driverNeededTip', { driver: driver?.name ?? conn.driver })}>
              <Badge tone="warning" hangul>
                {ws('driverNeeded')}
              </Badge>
            </Tooltip>
          )}
          <DropdownMenu items={menuItems}>
            <IconButton
              label={ws('more')}
              size="sm"
              onDoubleClick={(e) => e.stopPropagation()}
              className="sel-muted opacity-0 group-hover:opacity-100 focus-visible:opacity-100 data-[state=open]:opacity-100"
            >
              <Ellipsis size={14} strokeWidth={1.75} />
            </IconButton>
          </DropdownMenu>
        </div>
      </div>
    </ContextMenu>
  );
}
