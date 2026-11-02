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
export const STAGING_CONN_ID = 'c-staging';
/** A MySQL 4.0 server holding EUC-KR text (docs/mysql-compat.md): pre-4.1 protocol, `euc-kr`. */
export const LEGACY_CONN_ID = 'c-legacy';
/** A MySQL 8.0 server, write-allowed: the target of the legacy copy demo (docs/db-copy.md). */
export const MYSQL8_CONN_ID = 'c-mysql8';
/** Catalog ids (`mockCatalogId`) of the canned MySQL servers: 4.0 EUC-KR `shop` and 8.0 `app`. */
export const MYSQL_LEGACY_CATALOG = 'mysql-legacy';
export const MYSQL_CATALOG = 'mysql';

/** A connection with its environment tag (DESIGN.md §7.2). */
function tagged(c: Connection, tag: NonNullable<Connection['tag']>): Connection {
  return { ...c, tag };
}

export function mockConnections(): Connection[] {
  return [
    tagged({
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
    }, 'production'),
    tagged({
      id: LOCAL_CONN_ID,
      name: 'local (SQLite)',
      driver: 'sqlite',
      fields: { path: 'C:\\data\\local.db' },
      session: { fetchBatchRows: 65536 },
      savePassword: false,
      hasPassword: false,
      hasSshSecret: false,
    }, 'local'),
    tagged({
      id: STAGING_CONN_ID,
      name: 'staging (PostgreSQL)',
      driver: 'postgres',
      host: 'db-staging.internal',
      port: 5432,
      database: 'orders',
      user: 'readonly',
      fields: { sslmode: 'prefer' },
      session: { workMem: '64MB', statementTimeoutMs: BigInt(1_800_000) },
      outputDir: 'C:\\exports\\staging',
      savePassword: true,
      hasPassword: true,
      hasSshSecret: false,
    }, 'staging'),
  ];
}

/**
 * The legacy MySQL demo connection. Only the mock backend lists it (`conn_list`), so tests that
 * seed the store with `mockConnections()` keep their three rows.
 */
export function mockLegacyConnection(): Connection {
  return tagged({
    id: LEGACY_CONN_ID,
    name: 'legacy (MySQL 4.0, EUC-KR)',
    driver: 'mysql',
    host: 'legacy-db.internal',
    port: 3306,
    database: 'shop',
    user: 'report',
    fields: { sslmode: 'disable', protocol: 'legacy', encoding: 'euc-kr', allow_old_password: 'false' },
    session: {},
    outputDir: 'C:\\exports\\legacy',
    savePassword: true,
    hasPassword: true,
    hasSshSecret: false,
  }, 'testing');
}

/**
 * The MySQL 8.0 demo target of the legacy copy (docs/db-copy.md 7): write-allowed, tagged
 * development, browsing the `app` database. Like the legacy connection, only the mock backend
 * lists it.
 */
export function mockMysql8Connection(): Connection {
  return tagged({
    id: MYSQL8_CONN_ID,
    name: 'mysql8 (MySQL 8.0)',
    driver: 'mysql',
    host: 'mysql8.internal',
    port: 3306,
    database: 'app',
    user: 'copy',
    fields: { sslmode: 'prefer', protocol: 'auto', encoding: 'auto', allow_old_password: 'false' },
    session: {},
    outputDir: 'C:\\exports\\mysql8',
    savePassword: true,
    hasPassword: true,
    hasSshSecret: false,
    writeAllowed: true,
  }, 'development');
}

/** Whether a MySQL connection reaches the mock's pre-4.1 server (pinned `legacy`, or a legacy-* host). */
export function mockMysqlIsLegacy(conn: Pick<Connection, 'host' | 'fields'>): boolean {
  const protocol = (conn.fields.protocol ?? '').trim().toLowerCase();
  if (['legacy', '4.0', 'old', 'pre41'].includes(protocol)) return true;
  return (protocol === '' || protocol === 'auto') && /legacy/i.test(conn.host ?? '');
}

/**
 * Which canned catalog a connection browses: the seeded PostgreSQL and SQLite connections have
 * their own; other SQLite connections borrow the SQLite one, MySQL connections the MySQL 4.0
 * `shop` server (legacy) or the MySQL 8 `app` server, everything else the PostgreSQL one.
 */
export function mockCatalogId(conn: Pick<Connection, 'id' | 'driver' | 'host' | 'fields'>): string {
  if (conn.id === PROD_CONN_ID || conn.id === LOCAL_CONN_ID) return conn.id;
  if (conn.driver === 'sqlite') return LOCAL_CONN_ID;
  if (conn.driver === 'mysql') return mockMysqlIsLegacy(conn) ? MYSQL_LEGACY_CATALOG : MYSQL_CATALOG;
  return PROD_CONN_ID;
}

const isMysqlCatalog = (cid: string): boolean => cid === MYSQL_LEGACY_CATALOG || cid === MYSQL_CATALOG;

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
      // Mirrors drivers/registry.json (fields, defaults and `choices` of `mysql`).
      fields: ['host', 'port', 'database', 'user', 'password', 'sslmode', 'protocol', 'encoding', 'allow_old_password'],
      defaults: { port: '3306', sslmode: 'prefer', protocol: 'auto', encoding: 'auto', allow_old_password: 'false' },
      choices: {
        sslmode: ['disable', 'prefer', 'require', 'verify-full'],
        protocol: ['auto', 'modern', 'legacy'],
        encoding: ['auto', 'utf8', 'euc-kr', 'latin1'],
      },
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

// ---- MySQL servers (MySQL has no schemas: each database is its one schema, as in core) --------

/** One MySQL column as `SHOW COLUMNS` / `SHOW CREATE TABLE` describe it. */
interface MyCol {
  name: string;
  /** Column type as MySQL prints it (`int(11)`, `enum('A','a','B')`, `timestamp(14)`). */
  type: string;
  notNull?: boolean;
  /** `COLUMN_DEFAULT` (unquoted); `CURRENT_TIMESTAMP` is a keyword. */
  def?: string;
  pk?: boolean;
  ai?: boolean;
  unique?: boolean;
  fk?: string;
  comment?: string;
}

interface MyTable extends TableSeed {
  cols: MyCol[];
  /** Secondary keys: name → columns. */
  keys?: [string, string[]][];
}

const id11: MyCol = { name: 'id', type: 'int(11)', notNull: true, pk: true, ai: true };
const zeroDt = (name: string): MyCol => ({ name, type: 'datetime', notNull: true, def: '0000-00-00 00:00:00' });

/** MySQL 4.0.27, `character_set = euc_kr`, MyISAM: an online shop of the early 2000s. */
const LEGACY_SHOP: MyTable[] = [
  {
    name: 'customers',
    rows: 1_240_000,
    width: 210,
    comment: '회원 정보',
    cols: [
      id11,
      { name: 'email', type: 'varchar(255)', notNull: true, def: '', unique: true },
      { name: 'name', type: 'varchar(40)', notNull: true, def: '' },
      { name: 'nickname', type: 'varchar(40)', notNull: true, def: '' },
      { name: 'phone', type: 'varchar(20)' },
      { name: 'level', type: 'tinyint(4)', notNull: true, def: '1' },
      zeroDt('joined_at'),
      { name: 'updated', type: 'timestamp(14)', notNull: true },
    ],
  },
  {
    name: 'orders',
    rows: 8_600_000,
    width: 160,
    comment: '주문',
    cols: [
      id11,
      { name: 'customer_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.customers(id)' },
      { name: 'status', type: "enum('신규','결제','배송','취소')", notNull: true, def: '신규' },
      { name: 'total', type: 'decimal(12,2)', notNull: true, def: '0.00' },
      { name: 'ordered_at', type: 'timestamp(14)', notNull: true },
      zeroDt('shipped_at'),
      { name: 'memo', type: 'text' },
    ],
    keys: [
      ['idx_orders_customer', ['customer_id']],
      ['idx_orders_ordered', ['ordered_at']],
    ],
  },
  {
    name: 'order_items',
    rows: 21_400_000,
    width: 64,
    comment: '주문 상품',
    cols: [
      id11,
      { name: 'order_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.orders(id)' },
      { name: 'product_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.products(id)' },
      { name: 'qty', type: 'smallint(6)', notNull: true, def: '1' },
      { name: 'price', type: 'decimal(10,2)', notNull: true, def: '0.00' },
    ],
    keys: [['idx_items_order', ['order_id']]],
  },
  {
    name: 'products',
    rows: 52_000,
    width: 420,
    comment: '상품',
    cols: [
      id11,
      { name: 'category_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.categories(id)' },
      { name: 'name', type: 'varchar(100)', notNull: true, def: '' },
      { name: 'grade', type: "enum('A','a','B')", notNull: true, def: 'A' },
      { name: 'price', type: 'decimal(10,2)', notNull: true, def: '0.00' },
      { name: 'stock', type: 'int(11)', notNull: true, def: '0' },
      { name: 'description', type: 'text' },
      { name: 'released', type: 'date', notNull: true, def: '0000-00-00' },
    ],
    keys: [['idx_products_category', ['category_id']]],
  },
  {
    name: 'categories',
    rows: 340,
    width: 80,
    comment: '상품 분류',
    cols: [id11, { name: 'parent_id', type: 'int(11)', notNull: true, def: '0' }, { name: 'name', type: 'varchar(50)', notNull: true, def: '' }],
  },
  {
    name: 'reviews',
    rows: 1_900_000,
    width: 520,
    comment: '상품 후기',
    cols: [
      id11,
      { name: 'product_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.products(id)' },
      { name: 'customer_id', type: 'int(11)', notNull: true, def: '0', fk: 'shop.customers(id)' },
      { name: 'rating', type: 'tinyint(4)', notNull: true, def: '5' },
      { name: 'body', type: 'text' },
      { name: 'written', type: 'timestamp(14)', notNull: true },
    ],
    keys: [['idx_reviews_product', ['product_id']]],
  },
  {
    name: 'coupons',
    rows: 12_000,
    width: 90,
    comment: '쿠폰',
    cols: [
      id11,
      { name: 'code', type: 'varchar(20)', notNull: true, def: '', unique: true },
      { name: 'discount', type: 'decimal(5,2)', notNull: true, def: '0.00' },
      { name: 'expires', type: 'date', notNull: true, def: '0000-00-00' },
    ],
  },
  {
    name: 'notices',
    rows: 1_200,
    width: 2_400,
    comment: '공지사항',
    cols: [id11, { name: 'title', type: 'varchar(200)', notNull: true, def: '' }, { name: 'body', type: 'mediumtext' }, zeroDt('posted')],
  },
];

const LEGACY_BOARD: MyTable[] = [
  {
    name: 'posts',
    rows: 2_100_000,
    width: 900,
    comment: '게시글',
    cols: [
      id11,
      { name: 'author', type: 'varchar(40)', notNull: true, def: '' },
      { name: 'title', type: 'varchar(200)', notNull: true, def: '' },
      { name: 'body', type: 'text' },
      { name: 'hits', type: 'int(11)', notNull: true, def: '0' },
      { name: 'written', type: 'timestamp(14)', notNull: true },
    ],
  },
  {
    name: 'comments',
    rows: 6_300_000,
    width: 240,
    comment: '댓글',
    cols: [
      id11,
      { name: 'post_id', type: 'int(11)', notNull: true, def: '0', fk: 'board.posts(id)' },
      { name: 'author', type: 'varchar(40)', notNull: true, def: '' },
      { name: 'body', type: 'text' },
      zeroDt('written'),
    ],
    keys: [['idx_comments_post', ['post_id']]],
  },
  {
    name: 'attachments',
    rows: 410_000,
    width: 160,
    comment: '첨부 파일',
    cols: [
      id11,
      { name: 'post_id', type: 'int(11)', notNull: true, def: '0', fk: 'board.posts(id)' },
      { name: 'file_name', type: 'varchar(255)', notNull: true, def: '' },
      { name: 'size', type: 'int(11)', notNull: true, def: '0' },
    ],
  },
];

/** MySQL 8.0.36, utf8mb4, InnoDB: the copy demo's target server (`shop` does not exist yet). */
const MYSQL8_APP: MyTable[] = [
  {
    name: 'members',
    rows: 820_000,
    width: 240,
    comment: '회원',
    cols: [
      { name: 'id', type: 'bigint unsigned', notNull: true, pk: true, ai: true },
      { name: 'email', type: 'varchar(255)', notNull: true, unique: true, comment: '로그인 이메일' },
      { name: 'name', type: 'varchar(50)', notNull: true, comment: '이름' },
      { name: 'created_at', type: 'datetime', notNull: true, def: 'CURRENT_TIMESTAMP', comment: '가입 시각' },
    ],
  },
  {
    name: 'sessions',
    rows: 3_100_000,
    width: 180,
    comment: '로그인 세션',
    cols: [
      { name: 'id', type: 'char(36)', notNull: true, pk: true },
      { name: 'member_id', type: 'bigint unsigned', notNull: true, fk: 'app.members(id)', comment: '회원' },
      { name: 'expires_at', type: 'datetime', notNull: true, comment: '만료 시각' },
    ],
    keys: [['idx_sessions_member', ['member_id']]],
  },
  {
    name: 'audit_log',
    rows: 15_000_000,
    width: 300,
    comment: '감사 로그',
    cols: [
      { name: 'id', type: 'bigint unsigned', notNull: true, pk: true, ai: true },
      { name: 'action', type: 'varchar(64)', notNull: true, comment: '동작' },
      { name: 'detail', type: 'json' },
      { name: 'at', type: 'timestamp(3)', notNull: true, def: 'CURRENT_TIMESTAMP(3)' },
    ],
  },
];

function mysqlTables(cid: string, db: string): MyTable[] | undefined {
  if (cid === MYSQL_LEGACY_CATALOG) return db === 'shop' ? LEGACY_SHOP : db === 'board' ? LEGACY_BOARD : undefined;
  if (cid === MYSQL_CATALOG) return db === 'app' ? MYSQL8_APP : undefined;
  return undefined;
}

function mysqlTable(cid: string, ref: TableRef): MyTable | undefined {
  const db = ref.database ?? ref.schema ?? defaultDatabase(cid);
  return mysqlTables(cid, db)?.find((t) => t.name === ref.name);
}

/** The database a connection is "in" when a reference names none. */
function defaultDatabase(cid: string): string {
  if (cid === LOCAL_CONN_ID) return 'main';
  if (cid === MYSQL_LEGACY_CATALOG) return 'shop';
  if (cid === MYSQL_CATALOG) return 'app';
  return 'orders';
}

function seedsFor(connId: string, db: string, schema: string): TableSeed[] | undefined {
  if (isMysqlCatalog(connId)) return schema === db || schema === '' ? mysqlTables(connId, db) : undefined;
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
  if (connId === MYSQL_LEGACY_CATALOG) {
    return [
      { name: 'shop', sizeBytes: BigInt(6_200_000_000), encoding: 'euckr', collation: 'euckr_korean_ci', accessible: true, isCurrent: true },
      { name: 'board', sizeBytes: BigInt(2_900_000_000), encoding: 'euckr', collation: 'euckr_korean_ci', accessible: true, isCurrent: false },
    ];
  }
  if (connId === MYSQL_CATALOG) {
    return [{ name: 'app', sizeBytes: BigInt(9_400_000_000), encoding: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci', accessible: true, isCurrent: true }];
  }
  return undefined;
}

export function mockSchemas(connId: string, db: string): SchemaInfo[] | undefined {
  if (isMysqlCatalog(connId)) {
    const tables = mysqlTables(connId, db);
    return tables ? [{ name: db, tableCount: BigInt(tables.length) }] : undefined;
  }
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
  const db = ref.database ?? (isMysqlCatalog(connId) && ref.schema ? ref.schema : defaultDatabase(connId));
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

/** Generic columns of a MySQL table the fixtures do not describe. */
const MYSQL_GENERIC_COLS: MyCol[] = [
  id11,
  { name: 'name', type: 'varchar(100)', notNull: true, def: '' },
  { name: 'status', type: 'varchar(20)', def: 'active' },
  { name: 'amount', type: 'decimal(12,2)' },
  { name: 'created_at', type: 'datetime', notNull: true, def: '0000-00-00 00:00:00' },
  { name: 'payload', type: 'text' },
];

function mysqlCols(cid: string, ref: TableRef): MyCol[] {
  return mysqlTable(cid, ref)?.cols ?? MYSQL_GENERIC_COLS;
}

function mysqlColumnInfo(cols: MyCol[]): ColumnInfo[] {
  return cols.map((c, i) =>
    col(i + 1, c.name, c.type, {
      nullable: !c.notNull,
      primaryKey: !!c.pk,
      unique: !!c.pk || !!c.unique,
      ...(c.def !== undefined ? { default: c.def } : {}),
      ...(c.fk ? { foreignKey: c.fk } : {}),
      ...(c.comment ? { comment: c.comment } : {}),
    }),
  );
}

export function mockColumns(connId: string, ref: TableRef): ColumnInfo[] {
  if (isMysqlCatalog(connId)) return mysqlColumnInfo(mysqlCols(connId, ref));
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

const bq = (n: string): string => `\`${n.replace(/`/g, '``')}\``;

/** `PRIMARY`, one `UNIQUE KEY` per unique column, then the table's secondary keys. */
function mysqlIndexes(cid: string, ref: TableRef, t: TableInfo): IndexInfo[] {
  const cols = mysqlCols(cid, ref);
  const spec = mysqlTable(cid, ref);
  const base = Number(t.tableBytes ?? 0);
  const rows = Number(t.estimatedRows ?? 0);
  const out: IndexInfo[] = [];
  const pk = cols.filter((c) => c.pk).map((c) => c.name);
  if (pk.length) {
    out.push({ name: 'PRIMARY', columns: pk, unique: true, primary: true, kind: 'btree', include: [], sizeBytes: BigInt(Math.round(base * 0.1)), definition: `PRIMARY KEY (${pk.map(bq).join(',')})` });
  }
  for (const c of cols.filter((x) => x.unique && !x.pk)) {
    out.push({ name: c.name, columns: [c.name], unique: true, primary: false, kind: 'btree', include: [], sizeBytes: BigInt(Math.round(base * 0.08)), definition: `UNIQUE KEY ${bq(c.name)} (${bq(c.name)})` });
  }
  for (const [name, columns] of spec?.keys ?? []) {
    out.push({ name, columns, unique: false, primary: false, kind: 'btree', include: [], sizeBytes: BigInt(Math.round(base * 0.06)), scans: BigInt(Math.round(rows * 0.04)), definition: `KEY ${bq(name)} (${columns.map(bq).join(',')})` });
  }
  return out;
}

export function mockIndexes(connId: string, ref: TableRef): IndexInfo[] {
  const t = mockFindTable(connId, ref);
  if (!t || t.kind === 'view') return [];
  if (isMysqlCatalog(connId)) return mysqlIndexes(connId, ref, t);
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

/** One column line of `SHOW CREATE TABLE` (4.0 spells keywords in lower case, 8.0 upper case). */
function mysqlColumnDdl(c: MyCol, legacy: boolean): string {
  const parts = [`  ${bq(c.name)} ${c.type}`];
  if (c.notNull) parts.push('NOT NULL');
  if (c.ai) parts.push(legacy ? 'auto_increment' : 'AUTO_INCREMENT');
  else if (c.def !== undefined) {
    const kw = /^CURRENT_TIMESTAMP/.test(c.def);
    parts.push(`${legacy ? 'default' : 'DEFAULT'} ${kw ? c.def : `'${c.def.replace(/'/g, "''")}'`}`);
  } else if (!c.notNull && !legacy && !/text|blob|json/.test(c.type)) parts.push('DEFAULT NULL');
  else if (!c.notNull && legacy && !/text|blob/.test(c.type)) parts.push('default NULL');
  if (c.comment && !legacy) parts.push(`COMMENT '${c.comment.replace(/'/g, "''")}'`);
  return parts.join(' ');
}

/** `SHOW CREATE TABLE` as the server prints it: `TYPE=MyISAM` on 4.0, `ENGINE=InnoDB …` on 8.0. */
function mysqlDdl(cid: string, ref: TableRef, t: TableInfo | undefined): string {
  const legacy = cid === MYSQL_LEGACY_CATALOG;
  const cols = mysqlCols(cid, ref);
  const lines = cols.map((c) => mysqlColumnDdl(c, legacy));
  for (const ix of t ? mysqlIndexes(cid, ref, t) : []) {
    // 4.0 prints two spaces after PRIMARY KEY.
    lines.push(`  ${legacy && ix.primary ? ix.definition?.replace('PRIMARY KEY ', 'PRIMARY KEY  ') : ix.definition}`);
  }
  const comment = t?.comment ? ` COMMENT='${t.comment.replace(/'/g, "''")}'` : '';
  const tail = legacy ? `) TYPE=MyISAM${comment}` : `) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci${comment}`;
  return [`CREATE TABLE ${bq(ref.name)} (`, lines.join(',\n'), tail].join('\n');
}

export function mockDdl(connId: string, ref: TableRef): string {
  if (isMysqlCatalog(connId)) return mysqlDdl(connId, ref, mockFindTable(connId, ref));
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
  if (isMysqlCatalog(connId)) {
    const db = ref.database ?? ref.schema;
    const from = db ? `${bq(db)}.${bq(ref.name)}` : bq(ref.name);
    return `SELECT\n  ${mysqlCols(connId, ref).map((c) => bq(c.name)).join(',\n  ')}\nFROM ${from}`;
  }
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
