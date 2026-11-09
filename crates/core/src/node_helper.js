'use strict';
// BulkDB Node.js helper — injected via `node --require`.
// Exposes a global `bulkdb` object for easy DB access in script tabs.
//
// Supported drivers (npm install -g required):
//   postgresql → pg           (npm install -g pg)
//   mysql      → mysql2       (npm install -g mysql2)
//   sqlite     → better-sqlite3 (npm install -g better-sqlite3)
//   mssql      → mssql        (npm install -g mssql)
//
// Quick start:
//   const rows = await bulkdb.from('users').select('id','name').where({ active: 1 }).limit(10).fetch();
//   const all  = await bulkdb.query('SELECT * FROM orders WHERE total > ?', [1000]);
(function () {
  var DRV = process.env.BULKDB_DRIVER || '';
  var isPg  = DRV === 'postgresql' || DRV === 'postgres';
  var isMy  = DRV === 'mysql';
  var isSq  = DRV === 'sqlite';
  var isMss = DRV === 'mssql';

  var _conn = null;

  function requireDriver(pkg) {
    try {
      return require(pkg);
    } catch (e) {
      throw new Error(
        'BulkDB: Cannot load "' + pkg + '". Install it with:\n' +
        '  npm install -g ' + pkg + '\n' +
        'Then re-run the script.'
      );
    }
  }

  async function connect() {
    if (_conn) return _conn;

    if (isPg) {
      var pg = requireDriver('pg');
      var c = new pg.Client({
        host:     process.env.BULKDB_HOST,
        port:     process.env.BULKDB_PORT ? +process.env.BULKDB_PORT : 5432,
        database: process.env.BULKDB_DATABASE,
        user:     process.env.BULKDB_USER,
        password: process.env.BULKDB_PASSWORD,
      });
      await c.connect();
      _conn = {
        query: async function (sql, p) { return (await c.query(sql, p || [])).rows; },
        end:   function () { return c.end(); },
        ph:    function (i) { return '$' + (i + 1); },
      };
    } else if (isMy) {
      var mysql = requireDriver('mysql2/promise');
      var mc = await mysql.createConnection({
        host:     process.env.BULKDB_HOST,
        port:     process.env.BULKDB_PORT ? +process.env.BULKDB_PORT : 3306,
        database: process.env.BULKDB_DATABASE,
        user:     process.env.BULKDB_USER,
        password: process.env.BULKDB_PASSWORD,
      });
      _conn = {
        query: async function (sql, p) { var r = await mc.execute(sql, p || []); return r[0]; },
        end:   function () { return mc.end(); },
        ph:    function () { return '?'; },
      };
    } else if (isSq) {
      var Db = requireDriver('better-sqlite3');
      var db = new Db(process.env.BULKDB_PATH || process.env.BULKDB_DATABASE || ':memory:');
      _conn = {
        query: function (sql, p) { return Promise.resolve(db.prepare(sql).all(p || [])); },
        end:   function () { db.close(); },
        ph:    function () { return '?'; },
      };
    } else if (isMss) {
      var mssql = requireDriver('mssql');
      await mssql.connect({
        user:     process.env.BULKDB_USER,
        password: process.env.BULKDB_PASSWORD,
        database: process.env.BULKDB_DATABASE,
        server:   process.env.BULKDB_HOST || 'localhost',
        port:     process.env.BULKDB_PORT ? +process.env.BULKDB_PORT : 1433,
        options:  { trustServerCertificate: true },
      });
      _conn = {
        query: async function (sqlStr, p) {
          var req = mssql.request();
          (p || []).forEach(function (v, i) { req.input('p' + i, v); });
          return (await req.query(sqlStr)).recordset;
        },
        end:   function () { return mssql.close(); },
        ph:    function (i) { return '@p' + i; },
      };
    } else {
      throw new Error(
        'BulkDB: Unknown BULKDB_DRIVER "' + DRV + '".\n' +
        'Supported: postgresql, mysql, sqlite, mssql'
      );
    }
    return _conn;
  }

  // ---- Query builder ----
  function Q(table) {
    this._t   = table;
    this._c   = ['*'];
    this._w   = null;
    this._l   = null;
    this._o   = null;
    this._ord = null;
  }
  Q.prototype.select  = function () { this._c = arguments.length ? Array.from(arguments) : ['*']; return this; };
  Q.prototype.columns = Q.prototype.select;
  Q.prototype.where   = function (w)        { this._w   = w;               return this; };
  Q.prototype.limit   = function (n)        { this._l   = n;               return this; };
  Q.prototype.offset  = function (n)        { this._o   = n;               return this; };
  Q.prototype.orderBy = function (col, dir) { this._ord = col + ' ' + (dir || 'ASC'); return this; };

  Q.prototype._build = async function () {
    var conn   = await connect();
    var params = [];
    var sql    = 'SELECT ' + this._c.join(', ') + ' FROM ' + this._t;
    if (this._w) {
      var entries = Object.entries(this._w).filter(function (e) { return e[1] !== undefined; });
      if (entries.length) {
        sql += ' WHERE ' + entries.map(function (e) {
          params.push(e[1]);
          return e[0] + ' = ' + conn.ph(params.length - 1);
        }).join(' AND ');
      }
    }
    if (this._ord)  sql += ' ORDER BY ' + this._ord;
    if (this._l != null) sql += ' LIMIT '  + this._l;
    if (this._o != null) sql += ' OFFSET ' + this._o;
    return { sql: sql, params: params, conn: conn };
  };

  Q.prototype.fetch = async function () {
    var r = await this._build(); return r.conn.query(r.sql, r.params);
  };
  Q.prototype.fetchOne = async function () {
    this._l = 1; var rows = await this.fetch(); return rows[0] !== undefined ? rows[0] : null;
  };
  Q.prototype.count = async function () {
    var conn   = await connect();
    var params = [];
    var sql    = 'SELECT COUNT(*) AS cnt FROM ' + this._t;
    if (this._w) {
      var entries = Object.entries(this._w).filter(function (e) { return e[1] !== undefined; });
      if (entries.length) {
        sql += ' WHERE ' + entries.map(function (e) {
          params.push(e[1]);
          return e[0] + ' = ' + conn.ph(params.length - 1);
        }).join(' AND ');
      }
    }
    var rows = await conn.query(sql, params);
    return Number((rows[0] || {}).cnt || (rows[0] || {}).count || 0);
  };

  // ---- SQL guard: SELECT / WITH(CTE) only ----
  function guardSql(sql) {
    // Strip block comments and line comments, then check the first keyword.
    var stripped = sql
      .replace(/\/\*[\s\S]*?\*\//g, ' ')
      .replace(/--[^\n]*/g, ' ')
      .trim()
      .toUpperCase();
    if (!/^(SELECT|WITH)\b/.test(stripped)) {
      throw new Error(
        'BulkDB: 읽기 전용 모드 — SELECT / WITH(CTE) 쿼리만 허용됩니다.\n' +
        'UPDATE, DELETE, INSERT, DROP, CREATE, ALTER 등은 차단됩니다.'
      );
    }
  }

  // ---- Public API ----
  global.bulkdb = {
    driver: DRV,
    /** Build a SELECT query for `table`. Chain: .select() .where() .limit() .orderBy() .fetch() */
    from:   function (table) { return new Q(table); },
    /**
     * Run raw SQL — SELECT / WITH only. Any DML/DDL throws before hitting the DB.
     * `params` uses driver-native placeholders ($1 for PG, ? for MySQL/SQLite/MSSQL).
     */
    query:  async function (sql, params) {
      guardSql(sql);
      return (await connect()).query(sql, params);
    },
    /** Close the connection (called automatically on process exit). */
    close:  async function () { if (_conn) { await _conn.end(); _conn = null; } },
  };

  process.on('exit', function () { try { if (_conn) _conn.end(); } catch (_) {} });
})();
