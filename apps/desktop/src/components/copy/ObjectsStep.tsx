'use client';

// Step 2 개체 (docs/db-copy.md 7): 전체 / 테이블 선택; the table tree (schema → tables, 28px rows:
// checkbox, name, row estimate "추정", size) with search and 모두 선택 / 모두 해제; and the other
// objects as switches bound to `CopyObjectFlags` (with counts from the last plan when known).

import type { CopyObjectFlags, CopyObjectKind, TableInfo, TableRef } from 'ipc-types';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Button, Checkbox, EmptyState, RowEstimate, SearchField, Spinner, Switch } from '@/components/ui';
import { formatBytes } from '@/lib/format';
import { dbKey, schemaKey, useCatalogStore } from '@/stores/catalog';
import { needsSessionPassword, useConnectionsStore } from '@/stores/connections';
import { useCopyStore } from '@/stores/copy';
import { familyOf, selectionEmpty } from './model';
import { Caption, RadioList } from './parts';
import { cs, k } from './strings';

type FlagKey = keyof CopyObjectFlags;

const FLAGS: Record<'postgres' | 'mysql' | 'sqlite', FlagKey[]> = {
  postgres: ['views', 'routines', 'triggers', 'sequences', 'types', 'extensions', 'comments', 'owners', 'grants', 'analyze', 'refreshMatviews'],
  // No 'comments': the MySQL planner keeps COMMENTs inside the translated CREATE TABLE regardless,
  // so the switch would be a no-op (spec 7 lists 주석 for the PG group only).
  mysql: ['views', 'triggers', 'sequences', 'analyze'],
  sqlite: ['views', 'triggers', 'analyze'],
};

const FLAG_KINDS: Partial<Record<FlagKey, CopyObjectKind[]>> = {
  views: ['view', 'materialized_view'],
  routines: ['function'],
  triggers: ['trigger'],
  sequences: ['sequence_value', 'auto_increment'],
  types: ['type', 'domain'],
  extensions: ['extension'],
  comments: ['comment'],
  owners: ['owner'],
  grants: ['grant'],
  analyze: ['analyze'],
  refreshMatviews: ['refresh'],
};

const isBaseTable = (t: TableInfo) => t.kind === 'table' || t.kind === 'partitioned' || t.kind === 'foreign';

function refOf(t: TableInfo, pg: boolean): TableRef {
  return pg ? { schema: t.schema, name: t.name } : { name: t.name };
}

const sameRef = (a: TableRef, b: TableRef) => a.name === b.name && (a.schema ?? '') === (b.schema ?? '');

export function ObjectsStep() {
  const draft = useCopyStore((s) => s.wizard.draft);
  const plan = useCopyStore((s) => s.wizard.plan);
  const conn = useConnectionsStore((s) => s.connections.find((c) => c.id === draft.source.connId));
  const locked = useConnectionsStore((s) => needsSessionPassword(s, draft.source.connId));
  const family = familyOf(conn?.driver);
  const pg = family === 'postgres';
  const { connId, database } = draft.source;
  const schemas = useCatalogStore((s) => s.schemas[dbKey(connId, database)]);
  const tablesBySchema = useCatalogStore((s) => s.tables);
  const loading = useCatalogStore((s) => s.loading);
  const [query, setQuery] = useState('');

  const schemaNames = useMemo(() => {
    const names = (schemas ?? []).map((s) => s.name);
    return pg && draft.source.schema ? names.filter((n) => n === draft.source.schema) : names;
  }, [schemas, pg, draft.source.schema]);

  useEffect(() => {
    if (!connId || !database || locked) return;
    void useCatalogStore.getState().loadSchemas(connId, database).catch(() => undefined);
  }, [connId, database, locked]);
  useEffect(() => {
    if (!connId || !database || locked) return;
    for (const s of schemaNames) void useCatalogStore.getState().loadTables(connId, database, s).catch(() => undefined);
  }, [connId, database, locked, schemaNames]);

  const groups = schemaNames.map((s) => ({ schema: s, tables: (tablesBySchema[schemaKey(connId, database, s)] ?? []).filter(isBaseTable), loading: !!loading[schemaKey(connId, database, s)] }));
  const all = groups.flatMap((g) => g.tables);
  const busy = !schemas || groups.some((g) => g.loading);
  const selected: TableRef[] = draft.selection.kind === 'tables' ? draft.selection.tables : all.map((t) => refOf(t, pg));
  const isSelected = (t: TableInfo) => selected.some((r) => sameRef(r, refOf(t, pg)));
  const q = query.trim().toLowerCase();
  const visible = (t: TableInfo) => !q || t.name.toLowerCase().includes(q) || t.schema.toLowerCase().includes(q);
  const setTables = (tables: TableRef[]) => useCopyStore.getState().setDraft({ selection: { kind: 'tables', tables } });
  const toggle = (t: TableInfo, on: boolean) => {
    const ref = refOf(t, pg);
    setTables(on ? [...selected.filter((r) => !sameRef(r, ref)), ref] : selected.filter((r) => !sameRef(r, ref)));
  };
  const allRefs = all.map((t) => refOf(t, pg));
  // Switching to 'tables' before the list has loaded would seed an empty selection (`all` is []),
  // so every table reads unchecked once it loads. Remember to re-seed once the tables are in.
  const seedAllWhenLoaded = useRef(false);
  const chooseTables = () => {
    seedAllWhenLoaded.current = busy;
    setTables(allRefs);
  };
  const selectionKind = draft.selection.kind;
  useEffect(() => {
    if (seedAllWhenLoaded.current && !busy && selectionKind === 'tables') {
      seedAllWhenLoaded.current = false;
      useCopyStore.getState().setDraft({ selection: { kind: 'tables', tables: allRefs } });
    }
  }, [busy, allRefs, selectionKind]);
  // 모두 선택 / 모두 해제 act on the rows a search leaves visible, not the hidden ones.
  const visibleRefs = all.filter(visible).map((t) => refOf(t, pg));
  const showSchemaHeaders = pg || groups.length > 1;

  const flags = draft.options.objects;
  const setFlag = (key: FlagKey, v: boolean) => useCopyStore.getState().setDraft({ options: { ...draft.options, objects: { ...flags, [key]: v } } });
  const flagCount = (key: FlagKey): number | undefined => {
    const kinds = FLAG_KINDS[key];
    if (!plan || !kinds) return undefined;
    return plan.items.filter((i) => kinds.includes(i.kind)).length;
  };

  return (
    <div className="flex flex-col gap-4">
      {/* A radio row, not a SegmentedControl: the steps bar above is the only segmented control. */}
      <RadioList
        inline
        name="selection"
        label={cs('selectionAria')}
        value={draft.selection.kind}
        onChange={(v) => {
          if (v === 'all') useCopyStore.getState().setDraft({ selection: { kind: 'all' } });
          else chooseTables();
        }}
        options={[
          { value: 'all', label: cs('selectionAll') },
          { value: 'tables', label: cs('selectionTables') },
        ]}
      />

      {draft.selection.kind === 'all' ? (
        <div className="text-fg-secondary">{cs('allTablesNote')}</div>
      ) : (
        <section className="flex min-h-0 flex-col gap-2">
          <div className="flex items-center gap-2">
            <div className="w-64">
              <SearchField value={query} onChange={setQuery} placeholder={cs('searchTables')} aria-label={cs('searchTables')} />
            </div>
            <Button size="sm" variant="ghost" onClick={() => setTables([...selected.filter((r) => !visibleRefs.some((x) => sameRef(x, r))), ...visibleRefs])}>
              {cs('selectAll')}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setTables(selected.filter((r) => !visibleRefs.some((x) => sameRef(x, r))))}>
              {cs('selectNone')}
            </Button>
            {/* pr-4 = the rows' px-2 + the list's 8px scrollbar gutter: ends flush with the size column. */}
            <span className="ml-auto pr-4 text-sm tabular-nums text-fg-secondary">{cs('selectedCount', { n: selected.length, total: all.length })}</span>
          </div>
          <div className="max-h-[300px] overflow-auto border-y border-separator [scrollbar-gutter:stable]" role="group" aria-label={cs('selectionTables')}>
            {busy && all.length === 0 ? (
              <div className="flex h-7 items-center gap-2 px-2 text-fg-secondary">
                <Spinner size={12} decorative />
                {cs('loadingTables')}
              </div>
            ) : all.length === 0 ? (
              <EmptyState title={cs('noTables')} />
            ) : (
              groups.map((g) => {
                const rows = g.tables.filter(visible);
                if (rows.length === 0) return null;
                const allOn = g.tables.length > 0 && g.tables.every(isSelected);
                return (
                  <div key={g.schema}>
                    {showSchemaHeaders && (
                      <label className="sticky top-0 z-[1] flex h-6 items-center gap-2 bg-panel px-2 text-sm font-semibold text-fg-secondary">
                        <Checkbox
                          checked={allOn}
                          aria-label={cs('tableAria', { name: g.schema })}
                          onChange={(on) => {
                            const refs = g.tables.map((t) => refOf(t, pg));
                            const rest = selected.filter((r) => !refs.some((x) => sameRef(x, r)));
                            setTables(on ? [...rest, ...refs] : rest);
                          }}
                        />
                        {g.schema}
                      </label>
                    )}
                    {rows.map((t) => (
                      <label key={`${t.schema}.${t.name}`} className="flex h-7 items-center gap-2 border-t border-separator px-2 first:border-t-0 hover:bg-hover" style={{ paddingLeft: showSchemaHeaders ? 26 : 8 }}>
                        <Checkbox checked={isSelected(t)} onChange={(on) => toggle(t, on)} aria-label={cs('tableAria', { name: pg ? `${t.schema}.${t.name}` : t.name })} />
                        <span className="min-w-0 flex-1 truncate">{t.name}</span>
                        <RowEstimate rows={t.estimatedRows} variant="compact" showLabel className="text-sm text-fg-secondary" />
                        <span className="w-20 text-right text-sm tabular-nums text-fg-secondary">{formatBytes(t.totalBytes)}</span>
                      </label>
                    ))}
                  </div>
                );
              })
            )}
          </div>
          {all.length > 0 && !busy && selectionEmpty(draft) && (
            <div className="text-sm text-danger" role="alert">
              {cs('noTablesSelected')}
            </div>
          )}
        </section>
      )}

      {family && (
        <section className="flex flex-col gap-2">
          <Caption>{cs('otherObjects')}</Caption>
          <div className="grid grid-cols-2 gap-x-6 gap-y-1.5">
            {FLAGS[family].map((key) => {
              const label = key === 'sequences' && family === 'mysql' ? cs('flag.sequencesMysql') : cs(k(`flag.${key}`));
              const n = flagCount(key);
              return (
                <label key={key} className="flex min-h-(--h-control) items-start gap-2">
                  <Switch checked={flags[key]} onChange={(v) => setFlag(key, v)} aria-label={label} className="mt-[3px]" />
                  <span className="min-w-0">
                    <span>{label}</span>
                    {n !== undefined && <span className="ml-1.5 text-sm tabular-nums text-fg-tertiary">{cs('flagCount', { n })}</span>}
                    {key === 'sequences' && <span className="block text-sm text-fg-secondary">{cs('flag.sequencesHelp')}</span>}
                  </span>
                </label>
              );
            })}
          </div>
        </section>
      )}
    </div>
  );
}
