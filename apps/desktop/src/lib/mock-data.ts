// Deterministic fixtures for the in-browser fake backend (`./mock-ipc`). Nothing here is
// random: the same table always has the same estimate, columns and indexes so tests can rely on it.

import type {
  ColumnInfo,
  Connection,
  DatabaseInfo,
  DriverInfo,
  HistoryEntry,
  IndexInfo,
  SchemaInfo,
  Settings,
  TableInfo,
  TableKind,
  TableRef,
} from 'ipc-types';

export const PROD_CONN_ID = 'c-prod';
export const LOCAL_CONN_ID = 'c-local';

export function mockConnections(): Connection[] {
  return [
    {
      id: PROD_CONN_ID,
      name: 'prod (PostgreSQL)',
      driver: 'postgres',
      host: 'db-prod.internal',
      port: 5432,
      database: 'orders',
      user: 'readonly',
      fields: { sslmode: 'require' },
      ssh: {
        enabled: true,
        host: 'bastion.example.com',
        port: 22,
        user: 'deploy',
        auth: 'key',
        keyPath: '~/.ssh/id_ed25519',
        compression: true,
      },
      session: {
        workMem: '64MB',
        statementTimeoutMs: BigInt(3_600_000),
        lockTimeoutMs: BigInt(5_000),
        maxParallelWorkers: 0,
      },
      outputDir: 'C:\\exports',
      savePassword: true,
      hasPassword: true,
      hasSshSecret: false,
    },
    {
      id: LOCAL_CONN_ID,
      name: 'local (SQLite)',
      driver: 'sqlite',
      fields: { path: 'C:\\data\\local.db' },
      session: { fetchBatchRows: 65536 },
      savePassword: false,
      hasPassword: false,
      hasSshSecret: false,
    },
  ];
}

export function mockDrivers(): DriverInfo[] {
  return [
    {
      id: 'postgres',
      name: 'PostgreSQL',
      tier: 'builtin',
      available: true,
      fields: ['host', 'port', 'database', 'user', 'password', 'sslmode'],
      defaults: { port: '5432', sslmode: 'prefer' },
      sessionOpts: ['work_mem', 'statement_timeout', 'lock_timeout', 'max_parallel_workers_per_gather'],
      odbcDrivers: [],
    },
    {
      id: 'mysql',
      name: 'MySQL / MariaDB',
      tier: 'builtin',
      available: true,
      fields: ['host', 'port', 'database', 'user', 'password', 'sslmode'],
      defaults: { port: '3306', sslmode: 'prefer' },
      sessionOpts: ['max_execution_time', 'net_read_timeout', 'fetch_batch'],
      odbcDrivers: [],
    },
    {
      id: 'mssql',
      name: 'SQL Server',
      tier: 'builtin',
      available: true,
      fields: ['host', 'port', 'database', 'user', 'password', 'instance', 'encrypt', 'trust_server_certificate'],
      defaults: { port: '1433', encrypt: 'true', trust_server_certificate: 'false' },
      sessionOpts: ['lock_timeout', 'fetch_batch'],
      odbcDrivers: [],
    },
    {
      id: 'sqlite',
      name: 'SQLite',
      tier: 'builtin',
      available: true,
      fields: ['path'],
      defaults: {},
      sessionOpts: ['fetch_batch'],
      odbcDrivers: [],
    },
    {
      id: 'oracle',
      name: 'Oracle Database',
      tier: 'vendor-client',
      available: false,
      detail: 'oci.dll not found in %ORACLE_HOME%\\bin, C:\\oracle\\instantclient*, %PATH%',
      installLabel: 'Oracle Instant Client 21 (Basic)',
      installUrl: 'https://www.oracle.com/database/technologies/instant-client/winx64-64-downloads.html',
      installHint: 'Unzip Instant Client, add its folder to PATH (or set ORACLE_HOME), then rescan drivers.',
      fields: ['host', 'port', 'service_name', 'user', 'password'],
      defaults: { port: '1521' },
      sessionOpts: ['statement_timeout', 'fetch_batch'],
      odbcDrivers: [],
    },
    {
      id: 'odbc',
      name: 'ODBC (generic)',
      tier: 'odbc',
      available: true,
      fields: ['driver', 'connection_string', 'user', 'password'],
      defaults: {},
      sessionOpts: ['fetch_batch'],
      odbcDrivers: ['ODBC Driver 18 for SQL Server', 'PostgreSQL Unicode(x64)', 'MySQL ODBC 9.1 Unicode Driver'],
      libraryPath: 'C:\\Windows\\System32\\odbc32.dll',
    },
  ];
}

export function mockSettings(): Settings {
  return {
    maxConcurrentJobs: 2,
    defaultOutputDir: 'C:\\exports',
    csvCrlfDefault: true,
    logLevel: 'info',
    tunnelIdleSecs: 300,
    monitorIntervalMs: 3000,
    progressIntervalMs: 200,
    verifyAfterExport: false,
    ui: {},
  };
}

// ---- catalog -------------------------------------------------------------------------------

interface TableSeed {
  name: string;
  rows: number;
  /** Average row width in bytes, drives the size estimate. */
  width: number;
  kind?: TableKind;
  comment?: string;
}

const PUBLIC_TABLES: TableSeed[] = [
  { name: 'orders', rows: 48_200_000, width: 180, comment: 'One row per order' },
  { name: 'order_items', rows: 215_000_000, width: 96, comment: 'Line items; FK to orders' },
  { name: 'customers', rows: 3_400_000, width: 320 },
  { name: 'products', rows: 182_000, width: 540 },
  { name: 'payments', rows: 51_000_000, width: 140 },
  { name: 'shipments', rows: 44_800_000, width: 210 },
  { name: 'invoices', rows: 47_900_000, width: 260 },
  { name: 'addresses', rows: 5_100_000, width: 300 },
  { name: 'carts', rows: 12_600_000, width: 90 },
  { name: 'cart_items', rows: 38_000_000, width: 70 },
  { name: 'sessions', rows: 390_000_000, width: 120, comment: 'Web sessions, 90-day retention' },
  { name: 'events', rows: 1_250_000_000, width: 220, kind: 'partitioned', comment: 'Partitioned by month' },
  { name: 'audit_log', rows: 88_000_000, width: 410 },
  { name: 'users', rows: 12_400, width: 260 },
  { name: 'roles', rows: 18, width: 80 },
];

const ANALYTICS_TABLES: TableSeed[] = [
  { name: 'daily_sales', rows: 3_650, width: 150 },
  { name: 'monthly_sales', rows: 120, width: 150, kind: 'materialized_view' },
  { name: 'customer_cohorts', rows: 3_400_000, width: 110 },
  { name: 'product_stats', rows: 182_000, width: 200 },
  { name: 'funnel_events', rows: 420_000_000, width: 160 },
  { name: 'page_views', rows: 2_100_000_000, width: 130, kind: 'partitioned' },
  { name: 'v_active_customers', rows: 1_900_000, width: 320, kind: 'view' },
  { name: 'revenue_by_region', rows: 2_400, width: 120 },
  { name: 'retention', rows: 36_500, width: 90 },
  { name: 'churn_scores', rows: 3_400_000, width: 60 },
  { name: 'experiments', rows: 240, width: 400 },
  { name: 'experiment_assignments', rows: 61_000_000, width: 70 },
  { name: 'dim_date', rows: 7_305, width: 140 },
  { name: 'dim_product', rows: 182_000, width: 540 },
  { name: 'fact_orders', rows: 48_200_000, width: 130, kind: 'partitioned' },
];

const STAGING_TABLES: TableSeed[] = [
  { name: 'stg_orders', rows: 2_000_000, width: 180 },
  { name: 'stg_customers', rows: 150_000, width: 320 },
  { name: 'load_log', rows: 9_400, width: 260 },
];

const SQLITE_TABLES: TableSeed[] = [
  { name: 'sensors', rows: 240, width: 110 },
  { name: 'measurements', rows: 18_500_000, width: 48 },
  { name: 'readings_2024', rows: 9_200_000, width: 52 },
  { name: 'readings_2025', rows: 7_800_000, width: 52 },
];

function seedsFor(connId: string, db: string, schema: string): TableSeed[] | undefined {
  if (connId === PROD_CONN_ID) {
    if (db === 'orders') {
      if (schema === 'public') return PUBLIC_TABLES;
      if (schema === 'analytics') return ANALYTICS_TABLES;
    }
    if (db === 'analytics_dw') {
      if (schema === 'public') return ANALYTICS_TABLES;
      if (schema === 'staging') return STAGING_TABLES;
    }
    return undefined;
  }
  if (connId === LOCAL_CONN_ID && db === 'main' && schema === 'main') return SQLITE_TABLES;
  return undefined;
}

export function mockDatabases(connId: string): DatabaseInfo[] | undefined {
  if (connId === PROD_CONN_ID) {
    return [
      {
        name: 'orders',
        sizeBytes: BigInt(48 * 1024 ** 3),
        encoding: 'UTF8',
        collation: 'en_US.utf8',
        accessible: true,
        isCurrent: true,
      },
      {
        name: 'analytics_dw',
        sizeBytes: BigInt(120 * 1024 ** 3),
        encoding: 'UTF8',
        collation: 'en_US.utf8',
        accessible: true,
        isCurrent: false,
      },
    ];
  }
  if (connId === LOCAL_CONN_ID) {
    return [{ name: 'main', sizeBytes: BigInt(1_340_000_000), accessible: true, isCurrent: true }];
  }
  return undefined;
}

export function mockSchemas(connId: string, db: string): SchemaInfo[] | undefined {
  if (connId === PROD_CONN_ID) {
    if (db === 'orders') {
      return [
        { name: 'public', owner: 'app', tableCount: BigInt(PUBLIC_TABLES.length) },
        { name: 'analytics', owner: 'analytics', tableCount: BigInt(ANALYTICS_TABLES.length) },
      ];
    }
    if (db === 'analytics_dw') {
      return [
        { name: 'public', owner: 'analytics', tableCount: BigInt(ANALYTICS_TABLES.length) },
        { name: 'staging', owner: 'etl', tableCount: BigInt(STAGING_TABLES.length) },
      ];
    }
    return undefined;
  }
  if (connId === LOCAL_CONN_ID && db === 'main') return [{ name: 'main', tableCount: BigInt(SQLITE_TABLES.length) }];
  return undefined;
}

function toTableInfo(schema: string, s: TableSeed): TableInfo {
  const tableBytes = s.rows * s.width;
  const indexBytes = s.kind === 'view' ? 0 : Math.round(tableBytes * 0.35);
  return {
    schema,
    name: s.name,
    kind: s.kind ?? 'table',
    estimatedRows: BigInt(s.rows),
    tableBytes: BigInt(tableBytes),
    indexBytes: BigInt(indexBytes),
    totalBytes: BigInt(tableBytes + indexBytes),
    comment: s.comment,
  };
}

export function mockTables(connId: string, db: string, schema: string): TableInfo[] | undefined {
  const seeds = seedsFor(connId, db, schema);
  return seeds?.map((s) => toTableInfo(schema, s));
}

/** Find a table by reference; `database` defaults to the connection's current database. */
export function mockFindTable(connId: string, ref: TableRef): TableInfo | undefined {
  const db = ref.database ?? (connId === LOCAL_CONN_ID ? 'main' : 'orders');
  const schemas = ref.schema ? [ref.schema] : (mockSchemas(connId, db) ?? []).map((s) => s.name);
  for (const schema of schemas) {
    const t = mockTables(connId, db, schema)?.find((x) => x.name === ref.name);
    if (t) return t;
  }
  return undefined;
}

function col(
  position: number,
  name: string,
  dataType: string,
  extra: Partial<ColumnInfo> = {},
): ColumnInfo {
  return { position, name, dataType, nullable: true, primaryKey: false, unique: false, ...extra };
}

export function mockColumns(connId: string, ref: TableRef): ColumnInfo[] {
  const sqlite = connId === LOCAL_CONN_ID;
  const idType = sqlite ? 'INTEGER' : 'bigint';
  const tsType = sqlite ? 'TEXT' : 'timestamp with time zone';
  switch (ref.name) {
    case 'orders':
      return [
        col(1, 'id', 'bigint', { nullable: false, primaryKey: true, unique: true, default: "nextval('orders_id_seq')" }),
        col(2, 'customer_id', 'bigint', { nullable: false, foreignKey: 'public.customers(id)' }),
        col(3, 'status', 'text', { nullable: false, default: "'new'::text", comment: 'new, paid, shipped, cancelled' }),
        col(4, 'total_cents', 'bigint', { nullable: false }),
        col(5, 'currency', 'character(3)', { nullable: false, default: "'KRW'::bpchar" }),
        col(6, 'placed_at', 'timestamp with time zone', { nullable: false, default: 'now()' }),
        col(7, 'shipped_at', 'timestamp with time zone'),
        col(8, 'note', 'text'),
        col(9, 'meta', 'jsonb', { default: "'{}'::jsonb" }),
      ];
    case 'order_items':
      return [
        col(1, 'id', 'bigint', { nullable: false, primaryKey: true, unique: true }),
        col(2, 'order_id', 'bigint', { nullable: false, foreignKey: 'public.orders(id)' }),
        col(3, 'product_id', 'integer', { nullable: false, foreignKey: 'public.products(id)' }),
        col(4, 'qty', 'integer', { nullable: false, default: '1' }),
        col(5, 'unit_price_cents', 'bigint', { nullable: false }),
        col(6, 'discount', 'numeric(5,2)', { default: '0' }),
      ];
    case 'customers':
      return [
        col(1, 'id', 'bigint', { nullable: false, primaryKey: true, unique: true }),
        col(2, 'email', 'character varying(255)', { nullable: false, unique: true }),
        col(3, 'name', 'text', { nullable: false }),
        col(4, 'phone', 'character varying(32)'),
        col(5, 'country', 'character(2)', { default: "'KR'::bpchar" }),
        col(6, 'created_at', 'timestamp with time zone', { nullable: false, default: 'now()' }),
        col(7, 'segment', 'text', { comment: 'marketing segment' }),
      ];
    case 'events':
      return [
        col(1, 'id', 'bigint', { nullable: false, primaryKey: true, unique: true }),
        col(2, 'occurred_at', 'timestamp with time zone', { nullable: false, primaryKey: true }),
        col(3, 'user_id', 'bigint'),
        col(4, 'kind', 'text', { nullable: false }),
        col(5, 'payload', 'jsonb', { nullable: false, default: "'{}'::jsonb" }),
        col(6, 'ip', 'inet'),
      ];
    case 'measurements':
    case 'readings_2024':
    case 'readings_2025':
      return [
        col(1, 'id', 'INTEGER', { nullable: false, primaryKey: true, unique: true }),
        col(2, 'sensor_id', 'INTEGER', { nullable: false, foreignKey: 'sensors(id)' }),
        col(3, 'ts', 'TEXT', { nullable: false }),
        col(4, 'value', 'REAL'),
        col(5, 'quality', 'INTEGER', { default: '0' }),
      ];
    default:
      return [
        col(1, 'id', idType, { nullable: false, primaryKey: true, unique: true }),
        col(2, 'name', sqlite ? 'TEXT' : 'text', { nullable: false }),
        col(3, 'status', sqlite ? 'TEXT' : 'text', { default: sqlite ? "'active'" : "'active'::text" }),
        col(4, 'amount', sqlite ? 'REAL' : 'numeric(12,2)'),
        col(5, 'created_at', tsType, { nullable: false, default: sqlite ? 'CURRENT_TIMESTAMP' : 'now()' }),
        col(6, 'updated_at', tsType),
        col(7, 'payload', sqlite ? 'TEXT' : 'jsonb'),
      ];
  }
}

export function mockIndexes(connId: string, ref: TableRef): IndexInfo[] {
  const t = mockFindTable(connId, ref);
  if (!t || t.kind === 'view') return [];
  const schema = ref.schema ?? t.schema;
  const name = ref.name;
  const sqlite = connId === LOCAL_CONN_ID;
  const q = (s: string) => (sqlite ? s : `${schema}.${s}`);
  // SQLite has no index access methods: its DDL has no `USING`.
  const using = sqlite ? '' : ' USING btree';
  const base = Number(t.tableBytes ?? 0);
  const pkey: IndexInfo = {
    name: `${name}_pkey`,
    columns: name === 'events' ? ['id', 'occurred_at'] : ['id'],
    unique: true,
    primary: true,
    kind: 'btree',
    include: [],
    sizeBytes: BigInt(Math.round(base * 0.12)),
    scans: BigInt(Math.round(Number(t.estimatedRows ?? 0) * 0.8)),
    definition: `CREATE UNIQUE INDEX ${name}_pkey ON ${q(name)}${using} (id)`,
  };
  if (name === 'orders') {
    return [
      pkey,
      {
        name: 'idx_orders_customer_id',
        columns: ['customer_id'],
        unique: false,
        primary: false,
        kind: 'btree',
        include: [],
        sizeBytes: BigInt(Math.round(base * 0.1)),
        scans: BigInt(12_440_201),
        definition: `CREATE INDEX idx_orders_customer_id ON ${q('orders')} USING btree (customer_id)`,
      },
      {
        name: 'idx_orders_open',
        columns: ['status', 'placed_at'],
        unique: false,
        primary: false,
        kind: 'btree',
        predicate: "(status <> 'cancelled'::text)",
        include: ['total_cents'],
        sizeBytes: BigInt(Math.round(base * 0.06)),
        scans: BigInt(884_112),
        definition: `CREATE INDEX idx_orders_open ON ${q('orders')} USING btree (status, placed_at) INCLUDE (total_cents) WHERE (status <> 'cancelled'::text)`,
      },
      {
        name: 'idx_orders_placed_at_brin',
        columns: ['placed_at'],
        unique: false,
        primary: false,
        kind: 'brin',
        include: [],
        sizeBytes: BigInt(1_200_000),
        scans: BigInt(5_118),
        definition: `CREATE INDEX idx_orders_placed_at_brin ON ${q('orders')} USING brin (placed_at)`,
      },
    ];
  }
  const second: IndexInfo = {
    name: `idx_${name}_created_at`,
    columns: name === 'order_items' ? ['order_id'] : ['created_at'],
    unique: false,
    primary: false,
    kind: 'btree',
    include: [],
    sizeBytes: BigInt(Math.round(base * 0.08)),
    scans: BigInt(Math.round(Number(t.estimatedRows ?? 0) * 0.05)),
    definition: `CREATE INDEX idx_${name}_created_at ON ${q(name)}${using} (${name === 'order_items' ? 'order_id' : 'created_at'})`,
  };
  return [pkey, second];
}

export function mockDdl(connId: string, ref: TableRef): string {
  const t = mockFindTable(connId, ref);
  const schema = ref.schema ?? t?.schema ?? 'public';
  const qualified = connId === LOCAL_CONN_ID ? `"${ref.name}"` : `"${schema}"."${ref.name}"`;
  const cols = mockColumns(connId, ref);
  const lines = cols.map((c) => {
    const parts = [`    "${c.name}" ${c.dataType}`];
    if (!c.nullable) parts.push('NOT NULL');
    if (c.default) parts.push(`DEFAULT ${c.default}`);
    return parts.join(' ');
  });
  const pk = cols.filter((c) => c.primaryKey).map((c) => `"${c.name}"`);
  if (pk.length) lines.push(`    PRIMARY KEY (${pk.join(', ')})`);
  const fks = cols
    .filter((c) => c.foreignKey)
    .map((c) => `    FOREIGN KEY ("${c.name}") REFERENCES ${c.foreignKey}`);
  const head = t?.kind === 'view' ? `CREATE VIEW ${qualified} AS\nSELECT ...;` : null;
  if (head) return head;
  const partition = t?.kind === 'partitioned' ? ' PARTITION BY RANGE (occurred_at)' : '';
  const idx = mockIndexes(connId, ref)
    .filter((i) => !i.primary && i.definition)
    .map((i) => `${i.definition};`);
  return [`CREATE TABLE ${qualified} (`, [...lines, ...fks].join(',\n'), `)${partition};`, ...idx].join('\n');
}

export function mockSelectSql(connId: string, ref: TableRef): string {
  const cols = mockColumns(connId, ref).map((c) => `"${c.name}"`);
  const t = mockFindTable(connId, ref);
  const schema = ref.schema ?? t?.schema;
  const from = connId === LOCAL_CONN_ID || !schema ? `"${ref.name}"` : `"${schema}"."${ref.name}"`;
  return `SELECT\n  ${cols.join(',\n  ')}\nFROM ${from}`;
}

// ---- history -------------------------------------------------------------------------------

export function mockHistory(): HistoryEntry[] {
  const base = Date.parse('2026-10-24T09:12:00+09:00');
  const mk = (
    i: number,
    connId: string,
    label: string,
    sql: string,
    state: HistoryEntry['state'],
    rows: number,
    durationMs: number,
    files: string[],
    error?: string,
  ): HistoryEntry => {
    const started = new Date(base - i * 5_400_000);
    const finished = new Date(started.getTime() + durationMs);
    return {
      id: BigInt(100 - i),
      jobId: `job-h${100 - i}`,
      connId,
      startedAt: started.toISOString(),
      finishedAt: finished.toISOString(),
      state,
      label,
      sql,
      session: connId === PROD_CONN_ID ? { workMem: '64MB', statementTimeoutMs: BigInt(3_600_000) } : { fetchBatchRows: 65536 },
      output: {
        dir: 'C:\\exports',
        namePattern: '{conn}_{name}_{yyyyMMdd_HHmm}',
        format: files[0]?.endsWith('.parquet') ? 'parquet' : 'csv',
        csv: { header: true, delimiter: ',', nullAs: '', bom: true, crlf: true },
        overwrite: false,
      },
      rows: BigInt(rows),
      bytes: BigInt(rows * 160),
      durationMs: BigInt(durationMs),
      files,
      error,
    };
  };
  return [
    mk(0, PROD_CONN_ID, 'orders_october', "SELECT * FROM public.orders WHERE placed_at >= '2026-10-01'", 'done', 4_120_344, 212_000, [
      'C:\\exports\\prod_orders_october_20261024_0912.csv',
    ]),
    mk(1, PROD_CONN_ID, 'customers_kr', "SELECT * FROM public.customers WHERE country = 'KR'", 'done', 2_980_112, 98_400, [
      'C:\\exports\\prod_customers_kr_20261024_0742.parquet',
    ]),
    mk(2, PROD_CONN_ID, 'events_sept', "SELECT * FROM public.events WHERE occurred_at >= '2026-09-01' AND occurred_at < '2026-10-01'", 'cancelled', 61_000_000, 1_420_000, [
      'C:\\exports\\prod_events_sept_20261024_0612.csv.partial',
    ], 'CANCELLED: Cancelled by user'),
    mk(3, LOCAL_CONN_ID, 'measurements', 'SELECT * FROM measurements', 'done', 18_500_000, 64_000, [
      'C:\\exports\\local_measurements_20261024_0442.csv',
    ]),
    mk(4, PROD_CONN_ID, 'audit_dump', 'SELECT * FROM public.audit_log', 'failed', 12_400_000, 388_000, [], 'IO: No space left on device (C:\\exports)'),
  ];
}
