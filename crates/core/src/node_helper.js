'use strict';
// BulkDB script helper, preloaded into every Node.js script tab with `node --require`.
// Defines the global `bulkdb`: read-only access to the saved connections, by name.
//
//   const a = bulkdb.db('kpia1');                       // a saved connection, by its name
//   const b = bulkdb.db('kpia2');                       // another one, open at the same time
//   const users  = await a.from('users').select('id', 'name').where({ status: 'active' }).fetch();
//   const orders = await b.from('orders').where({ user_id: users.map((u) => u.id) }).fetch();
//   const total  = await a.query('SELECT COUNT(*) AS n FROM users');
//
// `bulkdb.from()` / `bulkdb.query()` without a name use the connection of the tab.
// Drivers are the usual npm packages, installed once with `npm install -g`: pg (PostgreSQL),
// mysql2 (MySQL / MariaDB), mssql (SQL Server). SQLite needs nothing (Node's node:sqlite).
(function () {
  const fs = require('fs');
  const path = require('path');
  const zlib = require('zlib');
  const Module = require('module');

  // ---- connections handed over by the app (`script::script_env`) ---------------------------

  let CONNS = [];
  try {
    CONNS = JSON.parse(process.env.BULKDB_CONNECTIONS || '[]');
  } catch (_) {
    CONNS = [];
  }
  // The list carries passwords: keep it out of the environment of anything the script spawns.
  delete process.env.BULKDB_CONNECTIONS;
  const DEFAULT_ID = process.env.BULKDB_CONN_ID || '';

  // ---- module lookup ------------------------------------------------------------------------

  // `npm install -g` puts packages where a script in a temp directory cannot see them. Add those
  // directories to the CommonJS search path, as NODE_PATH would.
  function globalModuleDirs() {
    const exeDir = path.dirname(process.execPath);
    const dirs = [];
    if (process.platform === 'win32') {
      if (process.env.APPDATA) dirs.push(path.join(process.env.APPDATA, 'npm', 'node_modules'));
      dirs.push(path.join(exeDir, 'node_modules'));
    } else {
      dirs.push(path.join(exeDir, '..', 'lib', 'node_modules'), '/opt/homebrew/lib/node_modules', '/usr/local/lib/node_modules', '/usr/lib/node_modules');
    }
    if (process.env.PNPM_HOME) {
      const base = path.join(process.env.PNPM_HOME, 'global');
      try {
        for (const layout of fs.readdirSync(base)) dirs.push(path.join(base, layout, 'node_modules'));
      } catch (_) {
        // no pnpm global directory
      }
    }
    return dirs.filter((d) => {
      try {
        return fs.statSync(d).isDirectory();
      } catch (_) {
        return false;
      }
    });
  }

  const globalDirs = globalModuleDirs();
  if (globalDirs.length && typeof Module._initPaths === 'function') {
    process.env.NODE_PATH = [process.env.NODE_PATH].concat(globalDirs).filter(Boolean).join(path.delimiter);
    Module._initPaths();
  }
  // Scripts run as ES modules (top-level `await`), where `require` does not exist by itself.
  if (typeof globalThis.require === 'undefined' && process.argv[1]) {
    globalThis.require = Module.createRequire(path.resolve(process.argv[1]));
  }

  // An error the helper raises itself, as opposed to one that comes from a driver.
  function refusal(message) {
    return Object.assign(new Error(message), { refusal: true });
  }

  function loadDriver(pkg) {
    try {
      return require(pkg);
    } catch (e) {
      const name = pkg.split('/')[0];
      if (e && e.code === 'MODULE_NOT_FOUND' && String(e.message).includes(name)) {
        throw refusal('드라이버 패키지 "' + name + '"가 설치되어 있지 않습니다. 터미널에서 한 번 실행하세요: npm install -g ' + name);
      }
      throw e;
    }
  }

  // ---- read-only guard ----------------------------------------------------------------------

  // Every way the server may read string literals (MySQL depends on sql_mode, PostgreSQL on
  // standard_conforming_strings). A query has to pass under each reading, so text that one
  // reading takes for a string and another runs as SQL is never skipped.
  //   single: backslash escapes inside '...'; double: "..." is a string with backslash escapes.
  function readings(driver) {
    if (driver === 'mysql') return [{ single: true, double: true }, { single: true, double: false }, { single: false, double: false }];
    if (driver === 'postgres') return [{ single: false, double: false }, { single: true, double: false }];
    return [{ single: false, double: false }];
  }

  // The SQL with comments, string literals and quoted identifiers blanked out, upper-cased.
  // Only constructs the driver really has are skipped; anything doubtful stays visible.
  function skeleton(sql, driver, reading) {
    const mysql = driver === 'mysql';
    const n = sql.length;
    let out = '';
    let i = 0;
    const quoted = (close, backslash) => {
      i++;
      while (i < n) {
        if (backslash && sql[i] === '\\') i += 2;
        else if (sql[i] === close && sql[i + 1] === close) i += 2;
        else if (sql[i] === close) break;
        else i++;
      }
      i++;
      out += ' ';
    };
    while (i < n) {
      const c = sql[i];
      const d = sql[i + 1];
      if (c === '-' && d === '-' && (!mysql || i + 2 >= n || /\s/.test(sql[i + 2]))) {
        while (i < n && sql[i] !== '\n' && sql[i] !== '\r') i++;
      } else if (c === '#' && mysql) {
        while (i < n && sql[i] !== '\n') i++;
      } else if (c === '/' && d === '*') {
        if (mysql && /^\/\*M?!/.test(sql.slice(i, i + 4))) throw new Error('MySQL 실행 주석(/*! ... */)은 사용할 수 없습니다.');
        const end = sql.indexOf('*/', i + 2);
        i = end < 0 ? n : end + 2;
        out += ' ';
      } else if (c === "'") {
        const escapeString = driver === 'postgres' && /(^|[^A-Za-z0-9_$])[eE]$/.test(sql.slice(Math.max(0, i - 2), i));
        quoted("'", reading.single || escapeString);
      } else if (c === '"') {
        quoted('"', mysql && reading.double);
      } else if (c === '`' && (mysql || driver === 'sqlite')) {
        quoted('`', false);
      } else if (c === '[' && (driver === 'mssql' || driver === 'sqlite')) {
        const end = sql.indexOf(']', i + 1);
        i = end < 0 ? n : end + 1;
        out += ' ';
      } else if (c === '$' && driver === 'postgres') {
        const tag = /^\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$/.exec(sql.slice(i, i + 80));
        if (tag) {
          const end = sql.indexOf(tag[0], i + tag[0].length);
          i = end < 0 ? n : end + tag[0].length;
          out += ' ';
        } else {
          out += c;
          i++;
        }
      } else {
        out += c;
        i++;
      }
    }
    return out.toUpperCase();
  }

  const WRITES = /\b(INSERT|UPDATE|DELETE|MERGE|INTO|DROP|CREATE|ALTER|TRUNCATE|GRANT|REVOKE|EXEC|EXECUTE|CALL)\b/;

  // Scripts read: one SELECT / WITH statement, with no writing keyword anywhere in it.
  function guard(sql, driver) {
    for (const reading of readings(driver)) {
      const text = skeleton(sql, driver, reading).replace(/[\s;]+$/, '');
      const first = /^[\s(]*([A-Z]+)/.exec(text);
      if (!first || (first[1] !== 'SELECT' && first[1] !== 'WITH')) {
        throw new Error('읽기 전용입니다: SELECT / WITH 쿼리만 실행할 수 있습니다.');
      }
      if (text.includes(';')) throw new Error('읽기 전용입니다: 한 번에 한 문장만 실행할 수 있습니다 (";" 로 이어 쓴 문장은 차단됩니다).');
      const write = WRITES.exec(text);
      if (write) throw new Error('읽기 전용입니다: ' + write[1] + ' 은(는) 차단됩니다. SELECT / WITH 쿼리만 실행할 수 있습니다.');
    }
  }

  // ---- drivers --------------------------------------------------------------------------------
  // Each opener returns { query(sql, params) -> rows, close() }. Sessions are made read-only on
  // the server where the server can do that, so the guard above is not the only barrier.

  async function openPostgres(c, database) {
    const pg = loadDriver('pg');
    const mode = String(c.fields.sslmode || 'prefer').toLowerCase();
    const noVerify = { rejectUnauthorized: false };
    const attempts = mode === 'disable' || mode === 'allow' ? [false] : mode === 'prefer' ? [noVerify, false] : mode === 'require' ? [noVerify] : [true];
    let client;
    for (let k = 0; ; k++) {
      client = new pg.Client({ host: c.host, port: c.port || 5432, database, user: c.user, password: c.password, ssl: attempts[k], application_name: 'bulkdb-script' });
      client.on('error', () => {});
      try {
        await client.connect();
        break;
      } catch (e) {
        // sslmode=prefer: fall back to clear text only when the server has no TLS at all.
        if (k + 1 < attempts.length && /does not support SSL/i.test(String(e && e.message))) continue;
        throw e;
      }
    }
    await client.query('SET default_transaction_read_only = on').catch(() => {});
    return {
      // The extended protocol takes exactly one statement.
      query: async (sql, params) => (await client.query({ text: sql, values: params, queryMode: 'extended' })).rows,
      close: () => client.end(),
    };
  }

  async function openMysql(c, database) {
    if (String(c.fields.protocol || 'auto').toLowerCase() === 'legacy') {
      throw refusal('MySQL 4.0 이하(4.1 이전 프로토콜) 서버는 스크립트에서 열 수 없습니다. 이 서버의 데이터는 추출 기능으로 내보내세요.');
    }
    const mysql = loadDriver('mysql2/promise');
    const encoding = String(c.fields.encoding || 'auto').toLowerCase();
    if (encoding !== 'auto' && encoding !== 'utf8') {
      console.error('[bulkdb] 경고: "' + c.name + '" 연결은 서버 인코딩이 ' + encoding + ' 로 설정되어 있습니다. 스크립트는 UTF-8 연결만 쓰므로 한글이 깨져 보일 수 있습니다.');
    }
    const mode = String(c.fields.sslmode || 'prefer').toLowerCase();
    const ssl = mode === 'require' ? { rejectUnauthorized: false } : mode === 'verify-full' ? {} : undefined;
    const conn = await mysql.createConnection({ host: c.host, port: c.port || 3306, database: database || undefined, user: c.user, password: c.password || undefined, ssl });
    conn.on('error', () => {});
    // MySQL 5.6.5+ / MariaDB 10.0+; older servers reject the statement and keep the guard only.
    await conn.query('SET SESSION TRANSACTION READ ONLY').catch(() => {});
    return {
      query: async (sql, params) => (await conn.execute(sql, params))[0],
      close: () => conn.end(),
    };
  }

  async function openMssql(c, database) {
    const mssql = loadDriver('mssql');
    const f = c.fields;
    const pool = new mssql.ConnectionPool({
      server: c.host || 'localhost',
      port: f.instance ? undefined : c.port || 1433,
      database: database || undefined,
      user: c.user,
      password: c.password,
      options: { instanceName: f.instance || undefined, encrypt: f.encrypt !== 'false', trustServerCertificate: f.trust_server_certificate === 'true', appName: 'bulkdb-script' },
      // The driver's default gives up after 15 s; report queries run longer (Stop cancels them).
      requestTimeout: 0,
      pool: { max: 1, min: 0 },
    });
    pool.on('error', () => {});
    await pool.connect();
    return {
      query: async (sql, params) => {
        const request = pool.request();
        params.forEach((v, k) => request.input('p' + k, v));
        return (await request.query(sql)).recordset;
      },
      close: () => pool.close(),
    };
  }

  async function openSqlite(c) {
    const file = c.fields.path || c.database;
    if (!file) throw refusal('SQLite 파일 경로가 없습니다.');
    let Better = null;
    try {
      Better = require('better-sqlite3');
    } catch (_) {
      // not installed (or built for another Node): use the built-in module
    }
    let db;
    if (Better) {
      db = new Better(file, { readonly: true, fileMustExist: true });
    } else {
      let builtin;
      try {
        builtin = require('node:sqlite');
      } catch (_) {
        throw refusal('SQLite 연결에는 Node.js 22.13 이상이 필요합니다 (또는: npm install -g better-sqlite3).');
      }
      db = new builtin.DatabaseSync(file, { readOnly: true });
    }
    return {
      // node:sqlite rows have no prototype; plain objects print and compare like any other row.
      query: async (sql, params) => db.prepare(sql).all(...params).map((row) => Object.assign({}, row)),
      close: async () => db.close(),
    };
  }

  const OPENERS = { postgres: openPostgres, mysql: openMysql, mssql: openMssql, sqlite: openSqlite };

  async function openClient(c, database) {
    if (c.ssh) throw refusal('SSH 터널을 쓰는 연결은 아직 스크립트에서 열 수 없습니다.');
    const open = OPENERS[c.driver];
    if (!open) throw refusal('"' + c.driver + '" 드라이버는 스크립트에서 지원하지 않습니다 (지원: postgres, mysql, mssql, sqlite).');
    try {
      return await open(c, database);
    } catch (e) {
      if (!c.password && c.driver !== 'sqlite' && e && !e.refusal && typeof e.message === 'string') {
        e.message += ' (이 연결은 저장된 비밀번호가 없습니다. 앱에서 연결을 한 번 열어 비밀번호를 입력한 뒤 다시 실행해 보세요.)';
      }
      throw e;
    }
  }

  // ---- connection lifetime ------------------------------------------------------------------

  // An open socket keeps Node alive, so a finished script would never exit. Once no query has
  // been running for a moment, every connection is closed; a later query simply reconnects.
  const IDLE_CLOSE_MS = 250;
  const live = new Set();
  let busy = 0;
  let idleTimer = null;

  function enter() {
    busy++;
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = null;
  }

  function leave() {
    if (--busy > 0) return;
    idleTimer = setTimeout(closeAll, IDLE_CLOSE_MS);
    idleTimer.unref();
  }

  function closeAll() {
    idleTimer = null;
    return Promise.all(Array.from(live, (handle) => handle.close()));
  }

  // ---- a connection, by name ------------------------------------------------------------------

  class Db {
    constructor(conn, database, root) {
      this._conn = conn;
      this._database = database;
      this._root = root || this;
      this._client = null;
      this._uses = new Map();
    }

    /** The saved connection's name. */
    get name() {
      return this._conn.name;
    }

    get driver() {
      return this._conn.driver;
    }

    get database() {
      return this._database;
    }

    /** The same server, another database: `bulkdb.db('kpia').use('kpia2')`. */
    use(database) {
      if (this.driver === 'sqlite') throw new Error('[' + this.name + '] SQLite 연결에는 use() 를 쓸 수 없습니다.');
      if (typeof database !== 'string' || !database) throw new TypeError('use(데이터베이스): 이름을 문자열로 넘겨 주세요.');
      const root = this._root;
      if (!root._uses.has(database)) root._uses.set(database, new Db(root._conn, database, root));
      return root._uses.get(database);
    }

    /** Start a SELECT: `.select() .where() .orderBy() .limit() .offset()`, then `.fetch()`. */
    from(table) {
      return new Query(this, table);
    }

    /** Run one SELECT / WITH statement; `params` use the driver's own placeholders. */
    async query(sql, params) {
      enter();
      try {
        guard(String(sql), this.driver);
        if (!this._client) {
          this._client = openClient(this._conn, this._database);
          live.add(this);
        }
        let client;
        try {
          client = await this._client;
        } catch (e) {
          this._client = null;
          live.delete(this);
          throw e;
        }
        return await client.query(String(sql), params || []);
      } catch (e) {
        if (e && typeof e.message === 'string' && !e.message.startsWith('[')) e.message = '[' + this.name + '] ' + e.message;
        throw e;
      } finally {
        leave();
      }
    }

    async close() {
      const opening = this._client;
      this._client = null;
      live.delete(this);
      if (!opening) return;
      try {
        await (await opening).close();
      } catch (_) {
        // already closed, or it never opened
      }
    }
  }

  function wholeNumber(value, what) {
    if (!Number.isInteger(value) || value < 0) throw new TypeError(what + ' 은(는) 0 이상의 정수여야 합니다.');
    return value;
  }

  class Query {
    constructor(db, table) {
      this._db = db;
      this._table = table;
      this._cols = ['*'];
      this._where = {};
      this._order = [];
      this._limit = null;
      this._offset = null;
    }

    select(...cols) {
      const list = cols.flat();
      this._cols = list.length ? list : ['*'];
      return this;
    }

    columns(...cols) {
      return this.select(...cols);
    }

    /** `{ col: value }` is `=`, `{ col: [a, b] }` is `IN`, `{ col: null }` is `IS NULL`; all ANDed. */
    where(conditions) {
      Object.assign(this._where, conditions);
      return this;
    }

    orderBy(col, dir) {
      const direction = String(dir || 'ASC').toUpperCase();
      if (direction !== 'ASC' && direction !== 'DESC') throw new TypeError('orderBy 방향은 "asc" 또는 "desc" 여야 합니다.');
      this._order.push(col + ' ' + direction);
      return this;
    }

    limit(n) {
      this._limit = wholeNumber(n, 'limit');
      return this;
    }

    offset(n) {
      this._offset = wholeNumber(n, 'offset');
      return this;
    }

    _build(cols, limit, offset, ordered) {
      const driver = this._db.driver;
      const params = [];
      const placeholder = (value) => {
        params.push(value);
        return driver === 'postgres' ? '$' + params.length : driver === 'mssql' ? '@p' + (params.length - 1) : '?';
      };
      const conditions = [];
      for (const [col, value] of Object.entries(this._where)) {
        if (value === undefined) continue;
        if (value === null) conditions.push(col + ' IS NULL');
        else if (Array.isArray(value)) conditions.push(value.length ? col + ' IN (' + value.map(placeholder).join(', ') + ')' : '1 = 0');
        else conditions.push(col + ' = ' + placeholder(value));
      }
      const mssql = driver === 'mssql';
      let sql = 'SELECT ' + (mssql && limit !== null && offset === null ? 'TOP (' + limit + ') ' : '') + cols.join(', ') + ' FROM ' + this._table;
      if (conditions.length) sql += ' WHERE ' + conditions.join(' AND ');
      if (ordered && this._order.length) sql += ' ORDER BY ' + this._order.join(', ');
      if (mssql) {
        if (offset !== null) {
          if (!this._order.length) sql += ' ORDER BY (SELECT NULL)';
          sql += ' OFFSET ' + offset + ' ROWS' + (limit !== null ? ' FETCH NEXT ' + limit + ' ROWS ONLY' : '');
        }
      } else if (limit !== null || offset !== null) {
        // MySQL and SQLite have no OFFSET without LIMIT.
        const unlimited = driver === 'mysql' ? ' LIMIT 18446744073709551615' : driver === 'sqlite' ? ' LIMIT -1' : '';
        sql += limit !== null ? ' LIMIT ' + limit : unlimited;
        if (offset !== null) sql += ' OFFSET ' + offset;
      }
      return { sql, params };
    }

    /** All matching rows. */
    fetch() {
      const { sql, params } = this._build(this._cols, this._limit, this._offset, true);
      return this._db.query(sql, params);
    }

    /** The first matching row, or `null`. */
    async fetchOne() {
      const { sql, params } = this._build(this._cols, 1, this._offset, true);
      const rows = await this._db.query(sql, params);
      return rows.length ? rows[0] : null;
    }

    /** Number of matching rows (ignores limit / offset). */
    async count() {
      const { sql, params } = this._build(['COUNT(*) AS n'], null, null, false);
      const rows = await this._db.query(sql, params);
      return Number(rows[0].n);
    }

    // `await a.from('t').where(...)` runs the query, like `.fetch()`.
    then(resolve, reject) {
      return this.fetch().then(resolve, reject);
    }
  }

  // ---- export: rows -> CSV / XLSX -------------------------------------------------------------
  // The rows are whatever the script built in JS — a query result, or sums and statistics it
  // combined from several databases. Columns are the union of the row keys unless given.

  function columnsOf(rows, explicit) {
    if (explicit && explicit.length) return explicit.slice();
    const order = [];
    const seen = new Set();
    for (const row of rows) {
      if (!row || typeof row !== 'object') continue;
      for (const key of Object.keys(row)) {
        if (!seen.has(key)) {
          seen.add(key);
          order.push(key);
        }
      }
    }
    return order;
  }

  function cellText(value) {
    if (value === null || value === undefined) return '';
    if (value instanceof Date) return value.toISOString();
    if (typeof value === 'bigint') return value.toString();
    if (typeof value === 'object') return JSON.stringify(value);
    return String(value);
  }

  function toCsv(rows, file, opts) {
    opts = opts || {};
    if (!Array.isArray(rows)) throw refusal('toCsv(행, 파일): 행은 객체 배열이어야 합니다.');
    if (typeof file !== 'string' || !file) throw refusal('toCsv(행, 파일): 저장할 파일 경로를 적어 주세요.');
    const columns = columnsOf(rows, opts.columns);
    const sep = opts.delimiter || ',';
    const eol = opts.eol || '\r\n';
    const quote = (text) => (text.includes(sep) || /["\r\n]/.test(text) ? '"' + text.replace(/"/g, '""') + '"' : text);
    const line = (values) => values.map((v) => quote(cellText(v))).join(sep);
    const body = [line(columns)];
    for (const row of rows) body.push(line(columns.map((c) => (row ? row[c] : undefined))));
    // Excel on Windows reads UTF-8 (Korean included) correctly only with a BOM; opt out with bom:false.
    const bom = opts.bom === false ? '' : '﻿';
    fs.writeFileSync(file, bom + body.join(eol) + eol);
    return { file, rows: rows.length, columns };
  }

  // Minimal OOXML (.xlsx) writer: one worksheet per data set, inline strings, a deflated ZIP.
  // No npm package needed. Verified by reading the output back with a real xlsx reader.
  const CRC_TABLE = (() => {
    const table = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      table[n] = c;
    }
    return table;
  })();

  function crc32(buf) {
    let c = ~0;
    for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
    return (~c) >>> 0;
  }

  function zip(entries) {
    const parts = [];
    const central = [];
    let offset = 0;
    for (const entry of entries) {
      const name = Buffer.from(entry.name, 'utf8');
      const data = Buffer.isBuffer(entry.data) ? entry.data : Buffer.from(entry.data, 'utf8');
      const deflated = zlib.deflateRawSync(data);
      const crc = crc32(data);
      const local = Buffer.alloc(30);
      local.writeUInt32LE(0x04034b50, 0);
      local.writeUInt16LE(20, 4);
      local.writeUInt16LE(0x0800, 6); // UTF-8 names
      local.writeUInt16LE(8, 8); // deflate
      local.writeUInt16LE(0, 10); // mod time
      local.writeUInt16LE(0x21, 12); // mod date = 1980-01-01 (a valid DOS date; 0 is not)
      local.writeUInt32LE(crc, 14);
      local.writeUInt32LE(deflated.length, 18);
      local.writeUInt32LE(data.length, 22);
      local.writeUInt16LE(name.length, 26);
      local.writeUInt16LE(0, 28);
      parts.push(local, name, deflated);
      const cd = Buffer.alloc(46);
      cd.writeUInt32LE(0x02014b50, 0);
      cd.writeUInt16LE(20, 4);
      cd.writeUInt16LE(20, 6);
      cd.writeUInt16LE(0x0800, 8);
      cd.writeUInt16LE(8, 10);
      cd.writeUInt16LE(0, 12);
      cd.writeUInt16LE(0x21, 14);
      cd.writeUInt32LE(crc, 16);
      cd.writeUInt32LE(deflated.length, 20);
      cd.writeUInt32LE(data.length, 24);
      cd.writeUInt16LE(name.length, 28);
      cd.writeUInt32LE(0, 30);
      cd.writeUInt32LE(0, 34);
      cd.writeUInt32LE(0, 38);
      cd.writeUInt32LE(offset, 42);
      central.push(cd, name);
      offset += local.length + name.length + deflated.length;
    }
    const centralBuf = Buffer.concat(central);
    const end = Buffer.alloc(22);
    end.writeUInt32LE(0x06054b50, 0);
    end.writeUInt16LE(entries.length, 8);
    end.writeUInt16LE(entries.length, 10);
    end.writeUInt32LE(centralBuf.length, 12);
    end.writeUInt32LE(offset, 16);
    return Buffer.concat([...parts, centralBuf, end]);
  }

  function xmlText(value) {
    // Element text: escape & < >, drop characters XML 1.0 forbids (keep tab, LF, CR).
    return value.replace(/[&<>]/g, (ch) => (ch === '&' ? '&amp;' : ch === '<' ? '&lt;' : '&gt;')).replace(/[\x00-\x08\x0b\x0c\x0e-\x1f]/g, '');
  }

  function colLetter(index) {
    let n = index;
    let s = '';
    do {
      s = String.fromCharCode(65 + (n % 26)) + s;
      n = Math.floor(n / 26) - 1;
    } while (n >= 0);
    return s;
  }

  function sheetXml(columns, rows) {
    const cell = (ref, value) => {
      if (typeof value === 'number' && Number.isFinite(value)) return '<c r="' + ref + '"><v>' + value + '</v></c>';
      if (typeof value === 'boolean') return '<c r="' + ref + '" t="b"><v>' + (value ? 1 : 0) + '</v></c>';
      const text = cellText(value);
      if (text === '') return '<c r="' + ref + '"/>';
      return '<c r="' + ref + '" t="inlineStr"><is><t xml:space="preserve">' + xmlText(text) + '</t></is></c>';
    };
    const lines = [];
    lines.push('<row r="1">' + columns.map((c, i) => cell(colLetter(i) + '1', c)).join('') + '</row>');
    rows.forEach((row, r) => {
      const ref = r + 2;
      lines.push('<row r="' + ref + '">' + columns.map((c, i) => cell(colLetter(i) + ref, row ? row[c] : undefined)).join('') + '</row>');
    });
    return (
      '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
      '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
      '<sheetData>' +
      lines.join('') +
      '</sheetData></worksheet>'
    );
  }

  // Excel: sheet names are 1..31 chars, may not hold : \ / ? * [ ], and must be unique.
  function sheetNames(raw) {
    const used = new Set();
    return raw.map((name, i) => {
      let base = String(name == null ? '' : name).replace(/[:\\/?*[\]]/g, ' ').trim().slice(0, 31) || 'Sheet' + (i + 1);
      let unique = base;
      let n = 2;
      while (used.has(unique.toLowerCase())) unique = base.slice(0, 28) + ' (' + n++ + ')';
      used.add(unique.toLowerCase());
      return unique;
    });
  }

  function toXlsx(data, file, opts) {
    opts = opts || {};
    if (typeof file !== 'string' || !file) throw refusal('toXlsx(데이터, 파일): 저장할 파일 경로를 적어 주세요.');
    let input;
    if (Array.isArray(data)) {
      input = [{ name: opts.sheetName || 'Sheet1', rows: data, columns: opts.columns }];
    } else if (data && typeof data === 'object') {
      input = Object.keys(data).map((name) => ({ name, rows: data[name], columns: opts.columns && opts.columns[name] }));
    } else {
      throw refusal('toXlsx(데이터, 파일): 데이터는 객체 배열이거나 { 시트이름: 배열 } 형태여야 합니다.');
    }
    if (!input.length) throw refusal('toXlsx: 내보낼 시트가 없습니다.');
    const names = sheetNames(input.map((s) => s.name));
    const sheets = input.map((s, i) => {
      if (!Array.isArray(s.rows)) throw refusal('toXlsx: "' + names[i] + '" 시트의 데이터가 배열이 아닙니다.');
      return { name: names[i], rows: s.rows, columns: columnsOf(s.rows, s.columns) };
    });

    const rels = sheets.map((_, i) => '<Relationship Id="rId' + (i + 1) + '" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet' + (i + 1) + '.xml"/>').join('');
    const sheetTags = sheets.map((s, i) => '<sheet name="' + xmlText(s.name).replace(/"/g, '&quot;') + '" sheetId="' + (i + 1) + '" r:id="rId' + (i + 1) + '"/>').join('');
    const overrides = sheets.map((_, i) => '<Override PartName="/xl/worksheets/sheet' + (i + 1) + '.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>').join('');
    const entries = [
      {
        name: '[Content_Types].xml',
        data:
          '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
          '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
          '<Default Extension="xml" ContentType="application/xml"/>' +
          '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
          overrides +
          '</Types>',
      },
      {
        name: '_rels/.rels',
        data:
          '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
          '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
          '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>' +
          '</Relationships>',
      },
      {
        name: 'xl/workbook.xml',
        data:
          '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
          '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">' +
          '<sheets>' +
          sheetTags +
          '</sheets></workbook>',
      },
      {
        name: 'xl/_rels/workbook.xml.rels',
        data: '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' + rels + '</Relationships>',
      },
      ...sheets.map((s, i) => ({ name: 'xl/worksheets/sheet' + (i + 1) + '.xml', data: sheetXml(s.columns, s.rows) })),
    ];
    fs.writeFileSync(file, zip(entries));
    return { file, sheets: sheets.map((s) => ({ name: s.name, rows: s.rows.length, columns: s.columns })) };
  }

  // ---- public API -----------------------------------------------------------------------------

  const handles = new Map();

  function db(name, opts) {
    if (typeof name !== 'string' || !name) throw new TypeError('bulkdb.db(이름): 저장된 연결의 이름을 문자열로 넘겨 주세요.');
    const named = CONNS.filter((c) => c.name === name);
    if (named.length > 1) throw new Error('이름이 "' + name + '" 인 연결이 ' + named.length + '개입니다. id 로 지정해 주세요: ' + named.map((c) => c.id).join(', '));
    const conn = named[0] || CONNS.find((c) => c.id === name);
    if (!conn) throw new Error('"' + name + '" 연결을 찾을 수 없습니다. 저장된 연결: ' + (CONNS.map((c) => c.name).join(', ') || '(없음)'));
    if (!conn.granted) {
      throw new Error('"' + conn.name + '" 연결 정보는 이 실행에 전달되지 않았습니다. 스크립트 본문에 연결 이름을 문자열 그대로 적어 주세요. 예: bulkdb.db(' + JSON.stringify(conn.name) + ')');
    }
    if (!handles.has(conn.id)) handles.set(conn.id, new Db(Object.assign({ fields: {} }, conn), conn.database || undefined));
    const handle = handles.get(conn.id);
    return opts && opts.database ? handle.use(opts.database) : handle;
  }

  function tabDb() {
    if (!CONNS.some((c) => c.id === DEFAULT_ID)) throw new Error('이 탭의 연결 정보를 찾지 못했습니다. bulkdb.db(이름) 으로 연결을 지정해 주세요.');
    return db(DEFAULT_ID);
  }

  globalThis.bulkdb = {
    /** A saved connection by name (or id); `{ database }` picks another database on that server. */
    db,
    /** The saved connections: `[{ name, driver, available }]`. */
    get connections() {
      return CONNS.map((c) => ({ name: c.name, driver: c.driver, available: !!c.granted }));
    },
    /** Name and driver of the tab's own connection. */
    name: process.env.BULKDB_CONN_NAME || '',
    driver: process.env.BULKDB_DRIVER || '',
    // The tab's own connection, without naming it.
    use: (database) => tabDb().use(database),
    from: (table) => tabDb().from(table),
    query: (sql, params) => tabDb().query(sql, params),
    /**
     * Write rows to a CSV file. `rows` is any array of objects the script built (a query result,
     * or sums / statistics combined from several databases). Options: { columns, delimiter, bom }.
     */
    toCsv,
    /**
     * Write to an .xlsx file (no package needed). `data` is an array of objects (one sheet) or
     * { 시트이름: 배열, ... } for several sheets — so two databases can go to separate sheets or a
     * combined summary. Options: { sheetName, columns }.
     */
    toXlsx,
    /** Close every open connection now (they also close by themselves when the script is done). */
    close: closeAll,
  };
})();
