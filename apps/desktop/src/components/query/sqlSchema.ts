// What the SQL editor knows about the target database: the lang-sql dialect for the connection's
// driver and a completion schema built from the catalog store. Pure functions over metadata the
// catalog store already holds: nothing here loads anything (the editor must never trigger catalog
// requests), and table data never appears here (there is none in this app).

import { MSSQL, MySQL, PLSQL, PostgreSQL, SQLite, StandardSQL, type SQLDialect, type SQLNamespace } from '@codemirror/lang-sql';
import { syntaxTree } from '@codemirror/language';
import type { ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind } from 'ipc-types';
import type { SqlCompletion, SqlCompletionSource } from './sqlCompletion';

// ---- dialects ------------------------------------------------------------------------------

export type DialectName = 'PostgreSQL' | 'MySQL' | 'MSSQL' | 'SQLite' | 'PLSQL' | 'StandardSQL';

const DIALECTS: Record<DialectName, SQLDialect> = { PostgreSQL, MySQL, MSSQL, SQLite, PLSQL, StandardSQL };

/**
 * Dialect for a driver id from `drivers/registry.json`. Oracle gets lang-sql's PL/SQL dialect
 * (Oracle keywords and `q'[...]'` strings); ODBC and unknown drivers use standard SQL.
 */
export function dialectNameFor(driver: string | undefined): DialectName {
  switch (driver) {
    case 'postgres':
      return 'PostgreSQL';
    case 'mysql':
      return 'MySQL';
    case 'mssql':
      return 'MSSQL';
    case 'sqlite':
      return 'SQLite';
    case 'oracle':
      return 'PLSQL';
    default:
      return 'StandardSQL';
  }
}

export function dialectFor(name: DialectName): SQLDialect {
  return DIALECTS[name];
}

// ---- catalog -> completion schema ----------------------------------------------------------

/** The parts of the catalog store the schema is built from (`CatalogState` satisfies it). */
export interface CatalogSnapshot {
  databases: Readonly<Record<string, readonly DatabaseInfo[]>>;
  schemas: Readonly<Record<string, readonly SchemaInfo[]>>;
  tables: Readonly<Record<string, readonly TableInfo[]>>;
  details: Readonly<Record<string, { columns?: readonly ColumnInfo[] }>>;
}

/** The connection the editor targets. */
export interface SchemaTarget {
  connId: string | null | undefined;
  driver?: string;
  /** The connection's configured database (its current database when connected). */
  database?: string;
  user?: string;
}

/** A table the query can name without a database qualifier. */
export interface SchemaTable {
  schema: string;
  name: string;
  kind?: TableKind;
  /** Present once the table's details were loaded (table tab / tree). */
  columns?: readonly ColumnInfo[];
}

export interface SqlSchema {
  /** For `sql({ schema })`: schema -> table -> columns (plus other databases on MSSQL). */
  namespace: SQLNamespace;
  /** Schema whose tables complete without a qualifier (`public`, `dbo`, `main`, ...). */
  defaultSchema?: string;
  /** Tables of the top-level schemas, for completing the columns of FROM / JOIN tables. */
  tables: readonly SchemaTable[];
}

const EMPTY_SCHEMA: SqlSchema = { namespace: {}, tables: [] };

type TableMap = Map<string, SchemaTable>;
type SchemaMap = Map<string, TableMap>;

function tablesOf(dbs: Map<string, SchemaMap>, db: string, schema: string): TableMap {
  let schemas = dbs.get(db);
  if (!schemas) dbs.set(db, (schemas = new Map()));
  let tables = schemas.get(schema);
  if (!tables) schemas.set(schema, (tables = new Map()));
  return tables;
}

/** lang-sql splits namespace keys on dots; a literal dot in a name is written `\.`. */
function nsKey(name: string): string {
  return name.replace(/\./g, '\\.');
}

/** Completion for an identifier, quoted the way lang-sql quotes names that need it. */
function ident(label: string, type: string, detail: string | undefined, dialect: SQLDialect): SqlCompletion {
  const open = dialect.spec.identifierQuotes?.[0] ?? '"';
  const close = open === '[' ? ']' : open;
  const plain = new RegExp('^[a-z_][a-z_\\d]*$', dialect.spec.caseInsensitiveIdentifiers ? 'i' : '').test(label);
  return plain ? { label, type, detail } : { label, type, detail, apply: `${open}${label.split(close).join(close + close)}${close}` };
}

function tableNamespace(tables: TableMap, dialect: SQLDialect): Record<string, SQLNamespace> {
  const out: Record<string, SQLNamespace> = {};
  for (const t of tables.values()) {
    out[nsKey(t.name)] = {
      self: ident(t.name, 'type', t.kind === 'table' || !t.kind ? 'table' : t.kind.replace(/_/g, ' '), dialect),
      children: (t.columns ?? []).map((c) => ident(c.name, 'property', c.dataType, dialect)),
    };
  }
  return out;
}

function pickDefaultSchema(driver: string | undefined, top: readonly string[], currentDb: string | undefined, user: string | undefined): string | undefined {
  const has = (s: string | undefined) => (s && top.includes(s) ? s : undefined);
  const single = top.length === 1 ? top[0] : undefined;
  switch (driver) {
    case 'postgres':
      return has('public') ?? single;
    case 'mssql':
      return has('dbo') ?? single;
    case 'sqlite':
      return has('main') ?? single;
    case 'mysql':
      return has(currentDb) ?? single;
    case 'oracle':
      return has(user?.toUpperCase()) ?? single;
    default:
      return single;
  }
}

/**
 * Build the completion schema for `target` from what the catalog store has loaded so far.
 *
 * Top level: the schemas of the connection's current database (on MySQL every loaded database,
 * since a MySQL database is the schema), each with its tables and, where details were loaded,
 * their columns. On MSSQL the other loaded databases are added as `db.schema.table` namespaces.
 * When the current database is unknown, the schemas of every loaded database are merged.
 */
export function buildSqlSchema(cat: CatalogSnapshot, target: SchemaTarget, dialect: SQLDialect = dialectFor(dialectNameFor(target.driver))): SqlSchema {
  const { connId, driver } = target;
  if (!connId) return EMPTY_SCHEMA;
  const prefix = `${connId}/`;
  const dbs = new Map<string, SchemaMap>();

  for (const [key, list] of Object.entries(cat.schemas)) {
    if (!key.startsWith(prefix)) continue;
    const db = key.slice(prefix.length);
    for (const s of list) tablesOf(dbs, db, s.name);
  }
  for (const [key, list] of Object.entries(cat.tables)) {
    if (!key.startsWith(prefix)) continue;
    const rest = key.slice(prefix.length);
    const cut = rest.indexOf('/');
    if (cut < 0) continue;
    const db = rest.slice(0, cut);
    const schema = rest.slice(cut + 1);
    const tables = tablesOf(dbs, db, schema);
    for (const t of list) tables.set(t.name, { schema, name: t.name, kind: t.kind });
  }

  const loaded = [...dbs.keys()];
  const listed = cat.databases[connId] ?? [];
  let currentDb = target.database && dbs.has(target.database) ? target.database : listed.find((d) => d.isCurrent)?.name;
  if (!currentDb && loaded.length === 1) currentDb = loaded[0];

  // Details keys are `connId/database/schema/name` (`lib/utils` tableKey).
  for (const [key, d] of Object.entries(cat.details)) {
    if (!key.startsWith(prefix) || !d.columns?.length) continue;
    const [dbPart, schema, ...nameParts] = key.slice(prefix.length).split('/');
    const name = nameParts.join('/');
    const db = dbPart || currentDb;
    if (!db || !schema || !name) continue;
    const tables = tablesOf(dbs, db, schema);
    const known = tables.get(name);
    tables.set(name, { ...(known ?? { schema, name }), columns: d.columns });
  }

  const topDbs = driver === 'mysql' || !currentDb ? [...dbs.keys()] : [currentDb];
  const top = new Map<string, TableMap>();
  for (const db of topDbs) {
    for (const [schema, tables] of dbs.get(db) ?? []) {
      const merged = top.get(schema);
      if (!merged) top.set(schema, new Map(tables));
      else for (const [n, t] of tables) if (!merged.has(n) || (t.columns && !merged.get(n)?.columns)) merged.set(n, t);
    }
  }

  const namespace: Record<string, SQLNamespace> = {};
  const reachable: SchemaTable[] = [];
  for (const [schema, tables] of top) {
    namespace[nsKey(schema)] = { self: ident(schema, 'namespace', 'schema', dialect), children: tableNamespace(tables, dialect) };
    reachable.push(...tables.values());
  }
  if (driver === 'mssql') {
    for (const [db, schemas] of dbs) {
      if (db === currentDb || nsKey(db) in namespace) continue;
      const children: Record<string, SQLNamespace> = {};
      for (const [schema, tables] of schemas) {
        children[nsKey(schema)] = { self: ident(schema, 'namespace', 'schema', dialect), children: tableNamespace(tables, dialect) };
      }
      namespace[nsKey(db)] = { self: ident(db, 'namespace', 'database', dialect), children };
    }
  }

  if (Object.keys(namespace).length === 0) return EMPTY_SCHEMA;
  return {
    namespace,
    defaultSchema: pickDefaultSchema(driver, [...top.keys()], currentDb, target.user),
    tables: reachable,
  };
}

// ---- columns of the tables a query reads ---------------------------------------------------

/** Statements longer than this are only scanned this far for FROM / JOIN tables. */
const MAX_SCAN = 200_000;
const ID = '(?:`[^`]+`|"[^"]+"|\\[[^\\]]+\\]|[\\w$#]+)';
const ID_RE = new RegExp(ID, 'g');
const TABLE_REF = new RegExp(`\\b(?:from|join)\\s+(${ID}(?:\\s*\\.\\s*${ID}){0,2})`, 'gi');
const SKIP_NODES = new Set(['String', 'QuotedIdentifier', 'LineComment', 'BlockComment']);

function unquote(part: string): string {
  return /^(`.*`|".*"|\[.*\])$/.test(part) ? part.slice(1, -1) : part;
}

/** `{ schema?, name }` for every `FROM x` / `JOIN x` (x = `t`, `s.t` or `db.s.t`) in `sql`. */
export function referencedTables(sql: string): { schema?: string; name: string }[] {
  const out: { schema?: string; name: string }[] = [];
  for (const m of sql.slice(0, MAX_SCAN).matchAll(TABLE_REF)) {
    const parts = (m[1].match(ID_RE) ?? []).map(unquote);
    const name = parts[parts.length - 1];
    if (!name) continue;
    out.push({ schema: parts.length > 1 ? parts[parts.length - 2] : undefined, name });
  }
  return out;
}

function sameName(a: string, b: string): boolean {
  return a === b || a.toLowerCase() === b.toLowerCase();
}

/** Tables of `schema` matching a reference: qualified, else in the default schema, else unique. */
function resolveTable(schema: SqlSchema, ref: { schema?: string; name: string }): SchemaTable[] {
  const named = schema.tables.filter((t) => sameName(t.name, ref.name));
  if (ref.schema) return named.filter((t) => sameName(t.schema, ref.schema!));
  const inDefault = named.filter((t) => t.schema === schema.defaultSchema);
  return inDefault.length > 0 ? inDefault : named.length === 1 ? named : [];
}

/**
 * Completion source for unqualified column names: the columns (already in the catalog store) of
 * the tables named after FROM / JOIN in the editor. Qualified names (`o.`, `public.orders.`) are
 * left to lang-sql's schema completion, which also resolves aliases.
 */
export function fromClauseColumnsSource(schema: SqlSchema, dialect: SQLDialect): SqlCompletionSource {
  const withColumns = schema.tables.some((t) => t.columns?.length);
  return (ctx) => {
    if (!withColumns) return null;
    if (SKIP_NODES.has(syntaxTree(ctx.state).resolveInner(ctx.pos, -1).name)) return null;
    const word = ctx.matchBefore(/[\w$]+/);
    if (!word && !ctx.explicit) return null;
    const from = word?.from ?? ctx.pos;
    if (ctx.state.sliceDoc(from - 1, from) === '.') return null;
    const options: SqlCompletion[] = [];
    const seen = new Set<string>();
    for (const ref of referencedTables(ctx.state.sliceDoc(0, MAX_SCAN))) {
      for (const table of resolveTable(schema, ref)) {
        for (const col of table.columns ?? []) {
          if (seen.has(col.name)) continue;
          seen.add(col.name);
          options.push({ ...ident(col.name, 'property', `${col.dataType} · ${table.name}`, dialect), boost: 1 });
        }
      }
    }
    return options.length > 0 ? { from, options, validFor: /^[\w$]*$/ } : null;
  };
}
