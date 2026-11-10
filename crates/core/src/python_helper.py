"""BulkDB script helper for Python, the counterpart of node_helper.js.

The shell runs a Python script tab as ``python <this file> <script.py>``: this file defines the
global ``bulkdb`` (read-only access to the saved connections, by name) and then execs the user's
script as ``__main__`` so tracebacks point at the real file and line.

    a = bulkdb.db('kpia1')                 # a saved connection, by its name
    b = bulkdb.db('kpia2')                 # another one, at the same time
    users  = a.from_('users').select('id', 'name').where({'status': 'active'}).fetch()
    orders = b.from_('orders').where({'user_id': [u['id'] for u in users]}).fetch()
    bulkdb.to_xlsx({'users': users, 'orders': orders}, 'out.xlsx')   # one sheet per database

``bulkdb.from_()`` / ``bulkdb.query()`` without a name use the tab's own connection. Rows come
back as plain dicts. Drivers: sqlite3 (built in), psycopg2 (PostgreSQL), pymysql (MySQL), pyodbc
(SQL Server) — install the one you need with pip.
"""

import atexit
import builtins
import csv
import datetime
import io
import json
import os
import re
import sys
import zipfile

# The app reads stdout/stderr as UTF-8 through a pipe; on Windows Python would otherwise encode
# them in the console code page and Korean would arrive garbled. (The shell also sets PYTHONUTF8.)
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

# ---- connections handed over by the app (script::script_env) --------------------------------

try:
    _CONNS = json.loads(os.environ.get("BULKDB_CONNECTIONS", "[]"))
except Exception:
    _CONNS = []
# Carries passwords: keep it out of the environment of anything the script starts.
os.environ.pop("BULKDB_CONNECTIONS", None)
_DEFAULT_ID = os.environ.get("BULKDB_CONN_ID", "")


class BulkdbError(Exception):
    """An error raised by the helper itself (read-only guard, missing driver, unknown name)."""


def _require(pkg, pip_name=None):
    try:
        return __import__(pkg)
    except ImportError:
        name = pip_name or pkg
        raise BulkdbError(
            '드라이버 패키지 "%s"가 설치되어 있지 않습니다. 터미널에서 한 번 실행하세요: pip install %s' % (name, name)
        )


# ---- read-only guard: SELECT / WITH only ----------------------------------------------------
# Mirrors node_helper.js. Every way the server may read string literals is checked, so text one
# reading treats as a string but another would run as SQL is never skipped.

def _readings(driver):
    if driver == "mysql":
        return [{"single": True, "double": True}, {"single": True, "double": False}, {"single": False, "double": False}]
    if driver == "postgres":
        return [{"single": False, "double": False}, {"single": True, "double": False}]
    return [{"single": False, "double": False}]


def _skeleton(sql, driver, reading):
    mysql = driver == "mysql"
    n = len(sql)
    out = []
    i = 0

    def quoted(start, close, backslash):
        j = start + 1
        while j < n:
            if backslash and sql[j] == "\\":
                j += 2
            elif sql[j] == close and j + 1 < n and sql[j + 1] == close:
                j += 2
            elif sql[j] == close:
                break
            else:
                j += 1
        return j + 1

    while i < n:
        c = sql[i]
        d = sql[i + 1] if i + 1 < n else ""
        if c == "-" and d == "-" and (not mysql or i + 2 >= n or sql[i + 2].isspace()):
            while i < n and sql[i] not in "\n\r":
                i += 1
        elif c == "#" and mysql:
            while i < n and sql[i] != "\n":
                i += 1
        elif c == "/" and d == "*":
            if mysql and re.match(r"/\*M?!", sql[i:i + 4]):
                raise BulkdbError("MySQL 실행 주석(/*! ... */)은 사용할 수 없습니다.")
            end = sql.find("*/", i + 2)
            i = n if end < 0 else end + 2
            out.append(" ")
        elif c == "'":
            escape_string = driver == "postgres" and bool(re.search(r"(^|[^A-Za-z0-9_$])[eE]$", sql[max(0, i - 2):i]))
            i = quoted(i, "'", reading["single"] or escape_string)
            out.append(" ")
        elif c == '"':
            i = quoted(i, '"', mysql and reading["double"])
            out.append(" ")
        elif c == "`" and (mysql or driver == "sqlite"):
            i = quoted(i, "`", False)
            out.append(" ")
        elif c == "[" and (driver == "mssql" or driver == "sqlite"):
            end = sql.find("]", i + 1)
            i = n if end < 0 else end + 1
            out.append(" ")
        elif c == "$" and driver == "postgres":
            tag = re.match(r"\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$", sql[i:i + 80])
            if tag:
                end = sql.find(tag.group(0), i + len(tag.group(0)))
                i = n if end < 0 else end + len(tag.group(0))
                out.append(" ")
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out).upper()


_WRITES = re.compile(r"\b(INSERT|UPDATE|DELETE|MERGE|INTO|DROP|CREATE|ALTER|TRUNCATE|GRANT|REVOKE|EXEC|EXECUTE|CALL)\b")


def _guard(sql, driver):
    for reading in _readings(driver):
        text = _skeleton(sql, driver, reading).rstrip("; \t\r\n")
        first = re.match(r"[\s(]*([A-Z]+)", text)
        if not first or first.group(1) not in ("SELECT", "WITH"):
            raise BulkdbError("읽기 전용입니다: SELECT / WITH 쿼리만 실행할 수 있습니다.")
        if ";" in text:
            raise BulkdbError('읽기 전용입니다: 한 번에 한 문장만 실행할 수 있습니다 (";" 로 이어 쓴 문장은 차단됩니다).')
        write = _WRITES.search(text)
        if write:
            raise BulkdbError("읽기 전용입니다: %s 은(는) 차단됩니다. SELECT / WITH 쿼리만 실행할 수 있습니다." % write.group(1))


# ---- drivers --------------------------------------------------------------------------------
# Each opener returns an object with query(sql, params) -> list[dict], close(), ph(i). Sessions
# are made read-only on the server where possible, so the guard above is not the only barrier.

def _fields(conn):
    return conn.get("fields") or {}


def _open_sqlite(conn):
    import sqlite3

    path = _fields(conn).get("path") or conn.get("database")
    if not path:
        raise BulkdbError("SQLite 파일 경로가 없습니다.")
    if path == ":memory:":
        db = sqlite3.connect(path)
    else:
        import pathlib

        uri = pathlib.Path(path).absolute().as_uri() + "?mode=ro"
        db = sqlite3.connect(uri, uri=True)
    db.row_factory = sqlite3.Row

    def query(sql, params):
        cur = db.execute(sql, list(params or []))
        rows = [dict(r) for r in cur.fetchall()]
        cur.close()
        return rows

    return {"query": query, "close": db.close, "ph": lambda i: "?"}


def _open_postgres(conn):
    psycopg2 = _require("psycopg2")
    import psycopg2.extras

    mode = str(_fields(conn).get("sslmode") or "prefer").lower()
    kw = dict(host=conn.get("host"), port=conn.get("port") or 5432, dbname=conn.get("database"),
              user=conn.get("user"), password=conn.get("password"), application_name="bulkdb-script")
    if mode and mode != "allow":
        kw["sslmode"] = mode
    db = psycopg2.connect(**kw)
    db.set_session(readonly=True, autocommit=True)

    def query(sql, params):
        cur = db.cursor(cursor_factory=psycopg2.extras.RealDictCursor)
        cur.execute(sql, list(params or []))
        rows = [dict(r) for r in cur.fetchall()]
        cur.close()
        return rows

    return {"query": query, "close": db.close, "ph": lambda i: "%s"}


def _open_mysql(conn):
    if str(_fields(conn).get("protocol") or "auto").lower() == "legacy":
        raise BulkdbError("MySQL 4.0 이하(4.1 이전 프로토콜) 서버는 스크립트에서 열 수 없습니다. 이 서버의 데이터는 추출 기능으로 내보내세요.")
    pymysql = _require("pymysql")
    encoding = str(_fields(conn).get("encoding") or "auto").lower()
    if encoding not in ("auto", "utf8"):
        sys.stderr.write('[bulkdb] 경고: "%s" 연결의 서버 인코딩이 %s 입니다. 스크립트는 UTF-8 연결만 쓰므로 한글이 깨져 보일 수 있습니다.\n' % (conn.get("name"), encoding))
    db = pymysql.connect(host=conn.get("host"), port=int(conn.get("port") or 3306), database=conn.get("database") or None,
                         user=conn.get("user"), password=conn.get("password") or "", cursorclass=pymysql.cursors.DictCursor)
    try:
        with db.cursor() as cur:
            cur.execute("SET SESSION TRANSACTION READ ONLY")
    except Exception:
        pass  # older servers: the guard stays the only barrier

    def query(sql, params):
        with db.cursor() as cur:
            cur.execute(sql, tuple(params or ()))
            return list(cur.fetchall())

    return {"query": query, "close": db.close, "ph": lambda i: "%s"}


def _open_mssql(conn):
    pyodbc = _require("pyodbc")
    f = _fields(conn)
    server = conn.get("host") or "localhost"
    if f.get("instance"):
        server += "\\" + f["instance"]
    elif conn.get("port"):
        server += "," + str(conn["port"])
    parts = ["DRIVER={ODBC Driver 17 for SQL Server}", "SERVER=" + server, "DATABASE=" + (conn.get("database") or ""),
             "UID=" + (conn.get("user") or ""), "PWD=" + (conn.get("password") or ""), "APP=bulkdb-script"]
    parts.append("Encrypt=yes" if f.get("encrypt") != "false" else "Encrypt=no")
    if f.get("trust_server_certificate") == "true":
        parts.append("TrustServerCertificate=yes")
    db = pyodbc.connect(";".join(parts), readonly=True)

    def query(sql, params):
        cur = db.cursor()
        cur.execute(sql, list(params or []))
        cols = [c[0] for c in cur.description]
        rows = [dict(zip(cols, row)) for row in cur.fetchall()]
        cur.close()
        return rows

    return {"query": query, "close": db.close, "ph": lambda i: "?"}


_OPENERS = {"sqlite": _open_sqlite, "postgres": _open_postgres, "mysql": _open_mysql, "mssql": _open_mssql}


def _open(conn):
    if conn.get("ssh"):
        raise BulkdbError("SSH 터널을 쓰는 연결은 아직 스크립트에서 열 수 없습니다.")
    opener = _OPENERS.get(conn.get("driver"))
    if not opener:
        raise BulkdbError('"%s" 드라이버는 스크립트에서 지원하지 않습니다 (지원: postgres, mysql, mssql, sqlite).' % conn.get("driver"))
    try:
        return opener(conn)
    except BulkdbError:
        raise
    except Exception as e:
        if not conn.get("password") and conn.get("driver") != "sqlite":
            raise type(e)("%s (이 연결은 저장된 비밀번호가 없습니다. 앱에서 연결을 한 번 열어 비밀번호를 입력한 뒤 다시 실행해 보세요.)" % e)
        raise


# ---- query builder --------------------------------------------------------------------------

def _whole(value, what):
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise TypeError("%s 은(는) 0 이상의 정수여야 합니다." % what)
    return value


class _Query(object):
    def __init__(self, db, table):
        self._db = db
        self._table = table
        self._cols = ["*"]
        self._where = {}
        self._order = []
        self._limit = None
        self._offset = None

    def select(self, *cols):
        flat = []
        for c in cols:
            flat.extend(c if isinstance(c, (list, tuple)) else [c])
        self._cols = list(flat) if flat else ["*"]
        return self

    columns = select

    def where(self, conditions=None, **kw):
        if conditions:
            self._where.update(conditions)
        if kw:
            self._where.update(kw)
        return self

    def order_by(self, col, direction="ASC"):
        d = str(direction).upper()
        if d not in ("ASC", "DESC"):
            raise TypeError('order_by 방향은 "asc" 또는 "desc" 여야 합니다.')
        self._order.append(col + " " + d)
        return self

    def limit(self, n):
        self._limit = _whole(n, "limit")
        return self

    def offset(self, n):
        self._offset = _whole(n, "offset")
        return self

    def _build(self, cols, limit, offset, ordered):
        driver = self._db.driver
        params = []

        def placeholder(value):
            params.append(value)
            return "%s" if driver in ("postgres", "mysql") else "?"

        conditions = []
        for col, value in self._where.items():
            if value is None:
                conditions.append(col + " IS NULL")
            elif isinstance(value, (list, tuple)):
                conditions.append((col + " IN (" + ", ".join(placeholder(v) for v in value) + ")") if len(value) else "1 = 0")
            else:
                conditions.append(col + " = " + placeholder(value))
        mssql = driver == "mssql"
        sql = "SELECT " + (("TOP (%d) " % limit) if (mssql and limit is not None and offset is None) else "") + ", ".join(cols) + " FROM " + self._table
        if conditions:
            sql += " WHERE " + " AND ".join(conditions)
        if ordered and self._order:
            sql += " ORDER BY " + ", ".join(self._order)
        if mssql:
            if offset is not None:
                if not self._order:
                    sql += " ORDER BY (SELECT NULL)"
                sql += " OFFSET %d ROWS" % offset + ((" FETCH NEXT %d ROWS ONLY" % limit) if limit is not None else "")
        elif limit is not None or offset is not None:
            unlimited = " LIMIT 18446744073709551615" if driver == "mysql" else (" LIMIT -1" if driver == "sqlite" else "")
            sql += (" LIMIT %d" % limit) if limit is not None else unlimited
            if offset is not None:
                sql += " OFFSET %d" % offset
        return sql, params

    def fetch(self):
        sql, params = self._build(self._cols, self._limit, self._offset, True)
        return self._db.query(sql, params)

    def fetch_one(self):
        sql, params = self._build(self._cols, 1, self._offset, True)
        rows = self._db.query(sql, params)
        return rows[0] if rows else None

    def count(self):
        sql, params = self._build(["COUNT(*) AS n"], None, None, False)
        return int(self._db.query(sql, params)[0]["n"])


class _Db(object):
    def __init__(self, conn):
        self._conn = conn
        self._client = None

    @property
    def name(self):
        return self._conn.get("name")

    @property
    def driver(self):
        return self._conn.get("driver")

    @property
    def database(self):
        return self._conn.get("database")

    def use(self, database):
        if self.driver == "sqlite":
            raise BulkdbError("[%s] SQLite 연결에는 use() 를 쓸 수 없습니다." % self.name)
        if not isinstance(database, str) or not database:
            raise TypeError("use(데이터베이스): 이름을 문자열로 넘겨 주세요.")
        clone = dict(self._conn)
        clone["database"] = database
        return _handle_for(clone, cache_key=self._conn.get("id", "") + "/" + database)

    def from_(self, table):
        return _Query(self, table)

    table = from_

    def query(self, sql, params=None):
        try:
            _guard(str(sql), self.driver)
            if self._client is None:
                self._client = _open(self._conn)
                _LIVE.append(self)
            return self._client["query"](str(sql), params or [])
        except Exception as e:
            msg = str(e)
            if msg.startswith("["):
                raise
            tagged = "[%s] %s" % (self.name, msg)
            try:
                new = type(e)(tagged)
            except Exception:
                new = BulkdbError(tagged)
            raise new from None

    def close(self):
        client = self._client
        self._client = None
        if self in _LIVE:
            _LIVE.remove(self)
        if client:
            try:
                client["close"]()
            except Exception:
                pass


_HANDLES = {}
_LIVE = []


def _handle_for(conn, cache_key):
    if cache_key not in _HANDLES:
        _HANDLES[cache_key] = _Db(conn)
    return _HANDLES[cache_key]


# ---- export: rows -> CSV / XLSX -------------------------------------------------------------

def _columns_of(rows, explicit):
    if explicit:
        return list(explicit)
    order = []
    seen = set()
    for row in rows:
        if isinstance(row, dict):
            for key in row.keys():
                if key not in seen:
                    seen.add(key)
                    order.append(key)
    return order


def _cell_text(value):
    if value is None:
        return ""
    if isinstance(value, (datetime.datetime, datetime.date)):
        return value.isoformat()
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def to_csv(rows, file, columns=None, delimiter=",", bom=True):
    if not isinstance(rows, (list, tuple)):
        raise BulkdbError("to_csv(행, 파일): 행은 dict 리스트여야 합니다.")
    cols = _columns_of(rows, columns)
    encoding = "utf-8-sig" if bom else "utf-8"  # Excel reads Korean UTF-8 correctly with a BOM
    with io.open(file, "w", newline="", encoding=encoding) as f:
        writer = csv.writer(f, delimiter=delimiter)
        writer.writerow(cols)
        for row in rows:
            writer.writerow([_cell_text((row or {}).get(c)) for c in cols])
    return {"file": file, "rows": len(rows), "columns": cols}


def _xml_text(value):
    out = value.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    return re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f]", "", out)


def _col_letter(index):
    n = index
    s = ""
    while True:
        s = chr(65 + (n % 26)) + s
        n = n // 26 - 1
        if n < 0:
            break
    return s


def _sheet_xml(columns, rows):
    def cell(ref, value):
        if isinstance(value, bool):
            return '<c r="%s" t="b"><v>%d</v></c>' % (ref, 1 if value else 0)
        if isinstance(value, (int, float)):
            return '<c r="%s"><v>%s</v></c>' % (ref, repr(value) if isinstance(value, float) else value)
        text = _cell_text(value)
        if text == "":
            return '<c r="%s"/>' % ref
        return '<c r="%s" t="inlineStr"><is><t xml:space="preserve">%s</t></is></c>' % (ref, _xml_text(text))

    lines = ['<row r="1">' + "".join(cell(_col_letter(i) + "1", c) for i, c in enumerate(columns)) + "</row>"]
    for r, row in enumerate(rows):
        ref = r + 2
        lines.append('<row r="%d">' % ref + "".join(cell(_col_letter(i) + str(ref), (row or {}).get(c)) for i, c in enumerate(columns)) + "</row>")
    return ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>'
            + "".join(lines) + "</sheetData></worksheet>")


def _sheet_names(raw):
    used = set()
    names = []
    for i, name in enumerate(raw):
        base = re.sub(r"[:\\/?*\[\]]", " ", "" if name is None else str(name)).strip()[:31] or ("Sheet%d" % (i + 1))
        unique = base
        n = 2
        while unique.lower() in used:
            unique = base[:28] + " (%d)" % n
            n += 1
        used.add(unique.lower())
        names.append(unique)
    return names


def to_xlsx(data, file, sheet_name=None, columns=None):
    if isinstance(data, (list, tuple)):
        inp = [(sheet_name or "Sheet1", list(data), columns)]
    elif isinstance(data, dict):
        inp = [(name, data[name], (columns or {}).get(name) if isinstance(columns, dict) else None) for name in data]
    else:
        raise BulkdbError("to_xlsx(데이터, 파일): 데이터는 dict 리스트이거나 { 시트이름: 리스트 } 형태여야 합니다.")
    if not inp:
        raise BulkdbError("to_xlsx: 내보낼 시트가 없습니다.")
    names = _sheet_names([s[0] for s in inp])
    sheets = []
    for i, (_, rows, cols) in enumerate(inp):
        if not isinstance(rows, (list, tuple)):
            raise BulkdbError('to_xlsx: "%s" 시트의 데이터가 리스트가 아닙니다.' % names[i])
        sheets.append((names[i], list(rows), _columns_of(rows, cols)))

    rels = "".join('<Relationship Id="rId%d" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet%d.xml"/>' % (i + 1, i + 1) for i in range(len(sheets)))
    sheet_tags = "".join('<sheet name="%s" sheetId="%d" r:id="rId%d"/>' % (_xml_text(s[0]).replace('"', "&quot;"), i + 1, i + 1) for i, s in enumerate(sheets))
    overrides = "".join('<Override PartName="/xl/worksheets/sheet%d.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>' % (i + 1) for i in range(len(sheets)))
    with zipfile.ZipFile(file, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("[Content_Types].xml",
                   '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                   '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
                   '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
                   '<Default Extension="xml" ContentType="application/xml"/>'
                   '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
                   + overrides + "</Types>")
        z.writestr("_rels/.rels",
                   '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                   '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
                   '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>'
                   "</Relationships>")
        z.writestr("xl/workbook.xml",
                   '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
                   '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
                   "<sheets>" + sheet_tags + "</sheets></workbook>")
        z.writestr("xl/_rels/workbook.xml.rels",
                   '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' + rels + "</Relationships>")
        for i, s in enumerate(sheets):
            z.writestr("xl/worksheets/sheet%d.xml" % (i + 1), _sheet_xml(s[2], s[1]))
    return {"file": file, "sheets": [{"name": s[0], "rows": len(s[1]), "columns": s[2]} for s in sheets]}


# ---- public API -----------------------------------------------------------------------------

class _Bulkdb(object):
    to_csv = staticmethod(to_csv)
    to_xlsx = staticmethod(to_xlsx)

    name = os.environ.get("BULKDB_CONN_NAME", "")
    driver = os.environ.get("BULKDB_DRIVER", "")

    def db(self, name, database=None):
        if not isinstance(name, str) or not name:
            raise TypeError("bulkdb.db(이름): 저장된 연결의 이름을 문자열로 넘겨 주세요.")
        named = [c for c in _CONNS if c.get("name") == name]
        if len(named) > 1:
            raise BulkdbError('이름이 "%s" 인 연결이 %d개입니다. id 로 지정해 주세요: %s' % (name, len(named), ", ".join(c.get("id") for c in named)))
        conn = named[0] if named else next((c for c in _CONNS if c.get("id") == name), None)
        if conn is None:
            raise BulkdbError('"%s" 연결을 찾을 수 없습니다. 저장된 연결: %s' % (name, ", ".join(c.get("name") for c in _CONNS) or "(없음)"))
        if not conn.get("granted"):
            raise BulkdbError('"%s" 연결 정보는 이 실행에 전달되지 않았습니다. 스크립트 본문에 연결 이름을 문자열 그대로 적어 주세요. 예: bulkdb.db(%r)' % (conn.get("name"), conn.get("name")))
        handle = _handle_for(conn, cache_key=conn.get("id", ""))
        return handle.use(database) if database else handle

    @property
    def connections(self):
        return [{"name": c.get("name"), "driver": c.get("driver"), "available": bool(c.get("granted"))} for c in _CONNS]

    def _tab(self):
        if not any(c.get("id") == _DEFAULT_ID for c in _CONNS):
            raise BulkdbError("이 탭의 연결 정보를 찾지 못했습니다. bulkdb.db(이름) 으로 연결을 지정해 주세요.")
        return self.db(_DEFAULT_ID)

    def use(self, database):
        return self._tab().use(database)

    def from_(self, table):
        return self._tab().from_(table)

    table = from_

    def query(self, sql, params=None):
        return self._tab().query(sql, params)

    def close(self):
        for handle in list(_LIVE):
            handle.close()


builtins.bulkdb = _Bulkdb()
atexit.register(builtins.bulkdb.close)


# ---- run the user script as __main__ --------------------------------------------------------

def _main():
    if len(sys.argv) < 2:
        return
    script = sys.argv[1]
    sys.argv = [script] + sys.argv[2:]
    script_dir = os.path.dirname(os.path.abspath(script))
    if script_dir not in sys.path:
        sys.path.insert(0, script_dir)
    with io.open(script, "r", encoding="utf-8") as f:
        source = f.read()
    code = compile(source, script, "exec")
    exec(code, {"__name__": "__main__", "__file__": script})


if __name__ == "__main__":
    _main()
