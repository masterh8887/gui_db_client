'use client';

// Step 1 원본·대상 (docs/db-copy.md 7): two columns, each a connection `Select` with its endpoint
// caption and EnvTag, a database `Select` from `catalog_databases` (MySQL targets may name a new
// database) and, on PostgreSQL, a schema `Select`. Only write-allowed connections of the source's
// family can be the target; the others are listed disabled ("쓰기 허용 아님") and can be marked
// write-allowed after a danger confirmation. "Ask every time" connections open DbPasswordDialog
// before their databases are listed (the password goes to core, never into wizard state).

import type { Connection, CopyEndpoint } from 'ipc-types';
import { PenLine } from 'lucide-react';
import { useEffect, useId, useRef, useState } from 'react';
import { Button, DropdownMenu, EnvTag, Field, Input, Notice, Select, type MenuItem, type SelectOption } from '@/components/ui';
import { DriverBadge } from '@/components/welcome/DriverBadge';
import { askConfirm } from '@/lib/confirm';
import { connTag } from '@/lib/env';
import { useCatalogStore, dbKey } from '@/stores/catalog';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { useUiStore } from '@/stores/ui';
import { driverLabel, endpointCaption, endpointProblem, familyOf, isDefaultOptions, isSameDatabase } from './model';
import { cs } from './strings';

type Side = 'source' | 'target';

const NEW = '\u0001new';
const ALL = '\u0001all';
const SAME = '\u0001same';

/** Ask core for the session password of an "ask every time" connection (DbPasswordDialog). */
function askPassword(connId: string) {
  useUiStore.getState().askDbPassword(connId, 'browse');
}

/** Confirm, then mark `conn` write-allowed; on success it becomes the target. */
export async function markWritable(conn: Connection): Promise<boolean> {
  const body = `${cs('markWritableBody')}${conn.tag === 'production' ? ` ${cs('markWritableProduction')}` : ''}`;
  const ok = await askConfirm({ title: cs('markWritableTitle', { name: conn.name }), body, confirmLabel: cs('markWritableConfirm'), danger: true });
  if (!ok) return false;
  const store = useCopyStore.getState();
  if (!(await store.markWriteAllowed(conn.id))) return false;
  const draft = useCopyStore.getState().wizard.draft;
  if (draft.target.connId !== conn.id) {
    store.setDraft({ target: { connId: conn.id, database: conn.driver === 'sqlite' ? 'main' : '' } });
  }
  return true;
}

/**
 * One connection in the connection Select and its trigger (spec 7): driver badge, name, EnvTag,
 * then why it cannot be picked. The driver's name is spoken; the badge is decoration.
 */
function ConnOption({ conn, note }: { conn: Connection; note?: string }) {
  const tag = connTag(conn);
  return (
    <span className="flex min-w-0 items-center gap-1.5" data-conn-option={conn.id}>
      <DriverBadge driverId={conn.driver} tag={tag} size="sm" />
      <span className="min-w-0 truncate">{conn.name}</span>
      <span className="sr-only">{` · ${driverLabel(conn.driver)}`}</span>
      {tag && <EnvTag tag={tag} variant="chip" />}
      {note && <span className="shrink-0 text-sm text-fg-tertiary">{note}</span>}
    </span>
  );
}

function SideColumn({ side }: { side: Side }) {
  const uid = useId();
  const draft = useCopyStore((s) => s.wizard.draft);
  const ep = draft[side];
  const connections = useConnectionsStore((s) => s.connections);
  const locked = useConnectionsStore((s) => needsSessionPassword(s, ep.connId));
  const conn = connections.find((c) => c.id === ep.connId);
  const source = connections.find((c) => c.id === draft.source.connId);
  const family = familyOf(source?.driver);
  const databases = useCatalogStore((s) => (ep.connId ? s.databases[ep.connId] : undefined));
  const dbLoading = useCatalogStore((s) => !!ep.connId && !!s.loading[ep.connId]);
  const dbError = useCatalogStore((s) => (ep.connId ? s.errors[ep.connId] : undefined));
  const schemas = useCatalogStore((s) => (ep.connId && ep.database ? s.schemas[dbKey(ep.connId, ep.database)] : undefined));
  const [newDb, setNewDb] = useState(false);
  const [newSchema, setNewSchema] = useState(false);
  const newDbRef = useRef<HTMLInputElement>(null);
  const focusNewDb = useRef(false);

  // Databases (and PG schemas) of the chosen connection, once its password is known.
  useEffect(() => {
    if (!ep.connId || locked) return;
    void useCatalogStore.getState().loadDatabases(ep.connId).catch(() => undefined);
  }, [ep.connId, locked]);
  useEffect(() => {
    if (!ep.connId || locked || !ep.database || conn?.driver !== 'postgres') return;
    if (databases && !databases.some((d) => d.name === ep.database)) return;
    void useCatalogStore.getState().loadSchemas(ep.connId, ep.database).catch(() => undefined);
  }, [ep.connId, ep.database, locked, conn?.driver, databases]);
  // The source must be an existing database: an unknown preset falls back to the current one.
  useEffect(() => {
    if (side !== 'source' || !databases || databases.length === 0) return;
    if (databases.some((d) => d.name === ep.database)) return;
    const pick = databases.find((d) => d.isCurrent && d.accessible) ?? databases.find((d) => d.accessible);
    if (pick) useCopyStore.getState().setDraft({ source: { ...ep, database: pick.name, schema: undefined } });
  }, [side, databases, ep]);

  const setEp = (next: CopyEndpoint) => {
    const patch: Partial<Record<Side, CopyEndpoint>> = { [side]: next };
    useCopyStore.getState().setDraft(patch);
  };

  const chooseConn = (connId: string) => {
    const c = connections.find((x) => x.id === connId);
    if (!c) return;
    setNewDb(false);
    setNewSchema(false);
    const database = c.driver === 'sqlite' ? 'main' : side === 'source' ? (c.database ?? '') : '';
    const store = useCopyStore.getState();
    if (side === 'source') {
      const target = connections.find((x) => x.id === draft.target.connId);
      store.setDraft({
        source: { connId, database },
        selection: { kind: 'all' },
        ...(target && target.driver !== c.driver ? { target: { connId: '', database: '' } } : {}),
      });
    } else {
      store.setDraft({ target: { connId, database } });
    }
    if (needsSessionPassword(useConnectionsStore.getState(), connId)) askPassword(connId);
  };

  const connOptions: SelectOption[] = connections
    .filter((c) => side === 'source' || !family || c.driver === family)
    .map((c) => {
      const supported = !!familyOf(c.driver);
      const writable = c.writeAllowed === true;
      const base = `${c.name} · ${driverLabel(c.driver)}`;
      if (!supported) return { value: c.id, label: cs('unsupportedDriver', { name: base }), content: <ConnOption conn={c} note={cs('unsupportedNote')} />, disabled: true };
      if (side === 'target' && !writable) {
        return { value: c.id, label: cs('notWritableOption', { name: base }), content: <ConnOption conn={c} note={cs('notWritableNote')} />, disabled: true };
      }
      return { value: c.id, label: base, content: <ConnOption conn={c} /> };
    });

  const mysqlTarget = side === 'target' && conn?.driver === 'mysql';
  const listed = (databases ?? []).filter((d) => d.accessible);
  const inList = listed.some((d) => d.name === ep.database);
  const showNewDb = mysqlTarget && (newDb || (!!ep.database && !!databases && !inList));
  const dbOptions: SelectOption[] = [
    ...listed.map((d) => ({ value: d.name, label: d.name })),
    ...(!inList && ep.database && !showNewDb ? [{ value: ep.database, label: ep.database }] : []),
    ...(mysqlTarget ? [{ value: NEW, label: cs('newDatabase') }] : []),
  ];

  const pg = conn?.driver === 'postgres';
  const sourceSchema = draft.source.schema;
  const schemaNames = (schemas ?? []).map((s) => s.name);
  const showSchema = pg && !!ep.database && (side === 'source' || !!sourceSchema);
  const schemaOptions: SelectOption[] =
    side === 'source'
      ? [{ value: ALL, label: cs('allSchemas') }, ...schemaNames.map((n) => ({ value: n, label: n }))]
      : [{ value: SAME, label: cs('sameSchemaName', { schema: sourceSchema ?? '' }) }, ...schemaNames.filter((n) => n !== sourceSchema).map((n) => ({ value: n, label: n })), { value: NEW, label: cs('newSchema') }];
  const schemaValue = side === 'source' ? (ep.schema ?? ALL) : newSchema || (ep.schema && !schemaNames.includes(ep.schema) && ep.schema !== sourceSchema) ? NEW : ep.schema && ep.schema !== sourceSchema ? ep.schema : SAME;

  const unwritable = side === 'target' ? connections.filter((c) => (!family || c.driver === family) && familyOf(c.driver) && c.writeAllowed !== true) : [];
  const markItems: MenuItem[] = unwritable.map((c) => ({
    id: c.id,
    label: (
      <span className="flex min-w-0 items-center gap-2">
        <span className="truncate">{c.name}</span>
        {connTag(c) && <EnvTag tag={connTag(c)} variant="chip" />}
      </span>
    ),
    onSelect: () => void markWritable(c),
  }));
  const sameFamily = side === 'target' && family ? connections.filter((c) => c.driver === family) : [];
  // On PostgreSQL / MySQL the source connection itself (with another database) is a valid target
  // once marked write-allowed, so a lone same-family connection is not a dead end there.
  const selfTargetable = family === 'postgres' || family === 'mysql';

  return (
    <section className="flex min-w-0 flex-col gap-3" aria-labelledby={`${uid}-h`}>
      <h3 id={`${uid}-h`} className="text-base font-semibold">
        {side === 'source' ? cs('source') : cs('target')}
      </h3>
      <Field label={cs('connection')} htmlFor={`${uid}-conn`}>
        <Select
          id={`${uid}-conn`}
          aria-label={`${side === 'source' ? cs('source') : cs('target')} ${cs('connection')}`}
          value={ep.connId || undefined}
          onChange={chooseConn}
          options={connOptions}
          placeholder={cs('chooseConnection')}
        />
        {conn && (
          <div className="flex min-w-0 items-center gap-1.5 text-sm text-fg-secondary">
            {/* The EnvTag is in the Select trigger itself. */}
            <span className="min-w-0 truncate" title={endpointCaption(conn, ep.database, ep.schema)} data-selectable>
              {endpointCaption(conn, ep.database, ep.schema)}
            </span>
            {side === 'target' && conn.writeAllowed === true && <PenLine size={12} strokeWidth={1.75} aria-label={cs('writeAllowedTip')} role="img" />}
          </div>
        )}
      </Field>

      {side === 'target' && conn && source && conn.driver !== source.driver && (
        <div className="text-sm text-danger" role="alert">
          {cs('familyMismatch', { driver: driverLabel(source.driver) })}
        </div>
      )}
      {side === 'target' && conn && conn.writeAllowed !== true && (
        <Notice tone="warning">
          <div>{cs('selectedNotWritable', { name: conn.name })}</div>
          <Button size="sm" variant="secondary" className="mt-1" onClick={() => void markWritable(conn)}>
            {cs('markWritable')}
          </Button>
        </Notice>
      )}
      {/* A chosen target that is not write-allowed has its own notice and button above. */}
      {side === 'target' && (!conn || (conn.writeAllowed === true && markItems.length > 0)) && (
        <div className="flex flex-col items-start gap-1 text-sm text-fg-secondary">
          {!conn && <span>{family && !selfTargetable && sameFamily.length <= 1 ? cs('noSameFamily', { driver: driverLabel(family) }) : cs('targetWritableHint')}</span>}
          {markItems.length > 0 && (
            <DropdownMenu items={markItems} align="start">
              <Button size="sm" variant="link" aria-label={cs('markWritableMenuAria')}>
                {cs('markWritable')}
              </Button>
            </DropdownMenu>
          )}
        </div>
      )}

      {conn && locked && (
        <Notice tone="info">
          <div>{cs('passwordNeeded')}</div>
          <Button size="sm" variant="secondary" className="mt-1" onClick={() => askPassword(conn.id)}>
            {cs('enterPassword')}
          </Button>
        </Notice>
      )}

      {conn && !locked && conn.driver !== 'sqlite' && (
        <Field label={cs('database')} htmlFor={`${uid}-db`} error={dbError}>
          <Select
            id={`${uid}-db`}
            aria-label={`${side === 'source' ? cs('source') : cs('target')} ${cs('database')}`}
            value={showNewDb ? NEW : ep.database || undefined}
            onChange={(v) => {
              if (v === NEW) {
                setNewDb(true);
                setEp({ connId: ep.connId, database: '' });
                focusNewDb.current = true;
                return;
              }
              setNewDb(false);
              setEp({ connId: ep.connId, database: v });
            }}
            options={dbOptions}
            placeholder={dbLoading ? cs('loadingDatabases') : cs('chooseDatabase')}
            onCloseAutoFocus={(e) => {
              // "새 데이터베이스 이름…" was picked: the name field gets the focus, not the trigger.
              if (!focusNewDb.current || !newDbRef.current) return;
              focusNewDb.current = false;
              e.preventDefault();
              newDbRef.current.focus();
            }}
          />
          {showNewDb && (
            <Input
              ref={newDbRef}
              aria-label={cs('newDatabaseName')}
              placeholder={cs('newDatabaseName')}
              value={ep.database}
              onChange={(e) => setEp({ connId: ep.connId, database: e.target.value })}
            />
          )}
          {showNewDb && <span className="text-sm text-fg-secondary">{cs('newDatabaseHint')}</span>}
        </Field>
      )}

      {showSchema && (
        <Field label={cs('schema')} htmlFor={`${uid}-schema`}>
          <Select
            id={`${uid}-schema`}
            aria-label={`${side === 'source' ? cs('source') : cs('target')} ${cs('schema')}`}
            value={schemaValue}
            onChange={(v) => {
              if (side === 'source') {
                const schema = v === ALL ? undefined : v;
                const patch = { source: { connId: ep.connId, database: ep.database, ...(schema ? { schema } : {}) } };
                // A target schema is only meaningful with a source schema.
                const t = draft.target;
                useCopyStore.getState().setDraft(schema ? patch : { ...patch, target: { connId: t.connId, database: t.database } });
                return;
              }
              setNewSchema(v === NEW);
              const schema = v === SAME || v === NEW ? undefined : v;
              setEp({ connId: ep.connId, database: ep.database, ...(schema ? { schema } : {}) });
            }}
            options={schemaOptions}
          />
          {side === 'target' && schemaValue === NEW && (
            <Input
              aria-label={cs('newSchemaName')}
              placeholder={cs('newSchemaName')}
              value={ep.schema ?? ''}
              onChange={(e) => setEp({ connId: ep.connId, database: ep.database, ...(e.target.value ? { schema: e.target.value } : {}) })}
            />
          )}
        </Field>
      )}
    </section>
  );
}

export function EndpointsStep() {
  const draft = useCopyStore((s) => s.wizard.draft);
  const connections = useConnectionsStore((s) => s.connections);
  const source = connections.find((c) => c.id === draft.source.connId);
  const target = connections.find((c) => c.id === draft.target.connId);
  const same = isSameDatabase(draft) && !!draft.source.database;
  const problem = endpointProblem(draft, connections);
  const direct = !problem && draft.selection.kind === 'all' && isDefaultOptions(draft.options);
  return (
    <div className="flex flex-col gap-4">
      <div className="grid grid-cols-2 gap-6">
        <SideColumn side="source" />
        <SideColumn side="target" />
      </div>
      {same && (
        <div className="text-base text-danger" role="alert">
          {cs('sameDatabase')}
        </div>
      )}
      {target?.tag === 'production' && source?.tag !== 'production' && <Notice tone="danger">{cs('productionTarget')}</Notice>}
      {direct && <div className="text-sm text-fg-secondary">{cs('directPlan')}</div>}
    </div>
  );
}
