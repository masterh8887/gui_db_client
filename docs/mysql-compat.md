# Legacy MySQL, EUC-KR servers and the CSV output encoding

Status: implemented in the core and wired into the desktop UI (section 9).
Request (user): connect to MySQL 4 (pre-4.1) and EUC-KR servers, with the version
differences handled, and export their data, converted to UTF-8 when wanted.

What changes:

1. **Legacy MySQL (3.23 / 4.0).** mysql_async 0.36 speaks only the 4.1 handshake, so BulkDB
   adds its own small pre-4.1 client. It is still pure Rust and sits in tier 1, so CLAUDE.md
   rule 4 holds.
2. **Old password hashes on 4.1–5.7.4 servers.** A new connection toggle switches on
   mysql_async's `secure_auth(false)`.
3. **Server encoding.** Text that a server holds as EUC-KR (CP949) is read correctly. That
   includes the Korean legacy setup where CP949 bytes sit in `latin1` columns.
4. **Output encoding.** A CSV can be written as UTF-8 (the default) or as EUC-KR (CP949).
   Parquet is always UTF-8.

The five CLAUDE.md rules still apply unchanged. Rows are never materialised, memory stays
fixed, cancel takes about 2 s, secrets are never logged (this covers the password and the
scramble), the CLI can do everything the UI can, and `cfg(target_os)` appears only in
`platform/`.

---

## 1. Ownership and integration order

| owner | files |
|---|---|
| **encoding** | `crates/core/src/encoding.rs`; `crates/core/src/sink/**`; `CsvOpts` + `CsvEncoding` (only) in `types.rs`; `preflight::check_output_encoding` (+ call sites in `preflight.rs` / `export.rs`); export flags in `crates/cli/src/args.rs` (`EncodingArg`, `ExportArgs::encoding`) and `cmd_export.rs`; `crates/core/Cargo.toml`; `crates/core/tests/sink_tests.rs`; `packages/ipc-types` (regenerate) |
| **legacy** | `crates/core/src/source/mysql_legacy/**`; `crates/core/tests/mysql_legacy_tests.rs`; `crates/core/tests/support/**` (fake server) |
| **modern** | `crates/core/src/source/mysql.rs`; `crates/core/src/source/mysql_common.rs` (the shared raw-query trait file); `catalog/mysql.rs`; `monitor/mysql.rs`; the mysql arm of `source/mod.rs`; `crates/core/tests/mysql_it.rs`; `drivers/registry.json`; `crates/core/src/drivers/**`; `.github/workflows/ci.yml`; `docker/compose.yml`; CLI help text for the new `--field` keys (`args.rs` top doc and `AdhocConn::fields` doc; disjoint from the export section) |
| **shared** (done in the skeleton) | `source/mysql_dispatch.rs`; `types.rs` outside `CsvOpts` (`DriverInfo.choices`); `lib.rs`; `source/mod.rs` module list; `SinkReport.unmappable_chars` + the `JobResult` note in `export.rs`; `ts_export.rs`; this document |

Integration order. Each step can be built and tested on its own:

1. **legacy** first ships `handshake::{parse_greeting, peek_greeting}`. Until that lands,
   `protocol = auto` fails with `INTERNAL "not implemented"`. That is acceptable for the
   branch; it is never merged in that state. `mysql_it.rs` calls `MySqlDatabase::connect`
   directly and is not affected.
2. **encoding** ships `encode_sql`, `decode_cell` (already usable), `repair_latin1_mojibake`,
   `Utf8Transcoder`/`TranscodeWriter` and the sink wiring. These do not depend on MySQL.
3. **modern** ships `MysqlRawQuery for MySqlDatabase`, the version-aware shared operations and
   the connect changes (§4).
4. **legacy** finishes the client and stream. Its catalog, monitor, probe and EXPLAIN work as
   soon as the shared operations from step 3 exist.

## 2. Version capability matrix

Single source of truth: `mysql_common::MysqlCaps::for_version` (unit-tested).

| capability | MySQL | MariaDB | used for |
|---|---|---|---|
| protocol 4.1 handshake (mysql_async) | 4.1.1+ | all | client choice |
| `SET NAMES`, per-connection charsets, `SHOW COLLATION`, `@@sql_mode` | 4.1+ | all | text plan, lexing |
| prepared statements (`COM_STMT_PREPARE`) | 4.1+ | all | modern multi-statement guard |
| `information_schema`, views, `KILL QUERY` | 5.0+ | all | catalog, cancel |
| `SHOW VARIABLES WHERE` | 5.0.3+ | all | probe |
| `information_schema.PROCESSLIST` | 5.1.7+ | all | monitor |
| `lock_wait_timeout`, `utf8mb4` | 5.5.3+ | 5.5+ | EXPLAIN timeouts, `SET NAMES` |
| `SET SESSION TRANSACTION READ ONLY` | 5.6.5+ | 10.0+ | extraction session |
| `EXPLAIN FORMAT=JSON` | 5.6.5+ | 10.1+ | plan |
| `max_execution_time` / `max_statement_time` | 5.7.8+ | 10.1+ | statement timeout |
| `EXPLAIN ANALYZE` / `ANALYZE FORMAT=JSON` | 8.0.18+ | 10.1+ | analyze |
| pre-4.1 password hashes can exist | < 5.7.5 | all | `allow_old_password` |
| `secure_auth` server default ON | 5.6.5+ | — | why old accounts fail on newer clients |

4.0 specifics: one server-wide charset (`SHOW VARIABLES LIKE 'character_set'`, values such as
`euc_kr` and `latin1`); `TIMESTAMP` values come without separators (`YYYYMMDDHHMMSS`);
`DECIMAL(M,D)` counts the sign and the point in `M`; there is no multi-statement support;
`KILL <id>` ends the whole session.

## 3. Connection settings (registry fields of `mysql`)

`drivers/registry.json` now lists `protocol`, `encoding` and `allow_old_password`, with
defaults and a new `choices` map. `DriverInfo.choices?` carries `choices` over IPC. Parsing
lives in `mysql_dispatch::MysqlConnOpts::from_params`, and an unknown value is `INVALID`.

| field | values (aliases) | default | label EN / KO |
|---|---|---|---|
| `protocol` | `auto`, `modern` (`4.1`, `new`), `legacy` (`4.0`, `old`, `pre41`) | `auto` | Protocol / 프로토콜 — Auto-detect / 자동 감지 · MySQL 4.1+ / MariaDB · MySQL 3.23–4.0 (pre-4.1) / MySQL 3.23–4.0 (4.1 이전) |
| `encoding` | `auto`, `utf8` (`utf-8`, `utf8mb4`), `euc-kr` (`euckr`, `euc_kr`, `cp949`), `latin1` (`cp1252`) | `auto` | Server encoding / 서버 인코딩 — Auto / 자동 · UTF-8 · EUC-KR (CP949) · latin1 |
| `allow_old_password` | `true` / `false` (also `1`/`0`, `yes`/`no`, `on`/`off`) | `false` | Allow old passwords (pre-4.1) / 구 버전(4.1 이전) 비밀번호 허용 — tooltip: "Needed for accounts created with OLD_PASSWORD(); weaker security." / "OLD_PASSWORD()로 만든 계정에 필요합니다. 보안이 약합니다." |

Every field value is validated (`INVALID` naming the field) before any socket is opened; a
mistyped `allow_old_password` is never silently `false`.

**How `protocol = auto` decides** (`mysql_dispatch::open`):

1. If this process already saw a 4.1+ greeting from `host:port`, connect with mysql_async
   straight away. When that connect fails, the cache entry is dropped.
2. Otherwise run `peek_greeting`: TCP connect and read the first packet.
   * An ERR packet there is mapped and returned: 1129 (host blocked) -> `CONNECT` with the
     hint to run `FLUSH HOSTS` / `mysqladmin flush-hosts`, 1130 (host not allowed) ->
     `CONNECT` with a grant / SSH-tunnel hint (`mysql_common::map_server_error`, shared by
     both clients, so mysql_async reports the same).
   * No `CLIENT_PROTOCOL_41` flag: the server is pre-4.1. The legacy handshake continues **on
     the same socket**.
   * Otherwise: answer the greeting with a credential-free login (`handshake::decline`, see
     below), remember the host as modern, connect with mysql_async. A 4.1 greeting without
     `CLIENT_SECURE_CONNECTION` (4.1.0 alpha) is refused by mysql_async.
3. Users who want to skip detection pin `modern` or `legacy`.

**Host-block rule: never drop a socket after the greeting without a login attempt.** MySQL
counts a client that disconnects after the greeting without a well-formed login packet (or
sends a malformed / too short one) toward `max_connect_errors` for its host (default 10; 4.0
`check_connection` -> `inc_host_errors`, 5.6+ host-cache handshake errors). Past the limit the
server answers every connection from that host, for every client on it, with 1129 until a DBA
runs `FLUSH HOSTS`. A failed login does not count (on 4.0 a well-formed packet even resets the
counter). With a wrong configuration a bare drop would happen on every attempt, so:

* **Validate before connecting** what needs no server: field values (every protocol); with
  an explicit `encoding`, whether the user name, password and database can be encoded in it
  (`mysql_legacy::precheck`); and TLS. This build has no TLS in either client (mysql_async
  is built without a TLS backend, `mysql::TLS_BACKEND = false`; the pre-4.1 client has none),
  and a TLS attempt by mysql_async ends right after the greeting (`NoClientSslFlagFromServer`
  or the missing backend) without a login, on every attempt. So `sslmode=require` /
  `verify-full` is a `CONNECT` error before any socket is opened, on every protocol:
  `mysql::check_tls_supported` in `MySqlDatabase::connect` (`modern`, also against 4.0) and in
  `mysql_dispatch::open` before the detection socket (`auto`), `mysql_legacy::precheck` for
  `legacy`. `sslmode=prefer` (the default) makes `MySqlDatabase::connect` go to clear text
  straight away instead of a TLS attempt followed by a clear-text retry.
* **Decline cleanly** what needs the greeting (`mysql_legacy::handshake::decline`): a 4.1+
  greeting on the detection socket, `protocol=legacy` against a 4.1+ server (also on lazy
  reconnects), login text that the greeting's charset cannot encode (and, only in a build
  with a TLS backend, TLS required but the server turned out to be pre-4.1 under `auto`). `decline` sends a well-formed login without any
  secret (user `bulkdb_nologin`, empty password, no database; `HandshakeResponse320` or
  `HandshakeResponse41`), reads the server's answer (ERR "access denied" -> close; OK ->
  `COM_QUIT`; a 4.1 old-password request -> a wrong dummy scramble, then the answer) and only
  then closes, bounded by 5 s. The user's real credentials never go over such a connection.
  `COM_QUIT` is not an option before authentication: the server reads it as a 1-byte (too
  short) login packet, which counts.
* The fake server counts interrupted handshakes (`FakeServer::interrupted_handshakes`); the
  failing-configuration tests (`failing_configs_never_interrupt_a_handshake`,
  `modern_client_never_interrupts_a_handshake`) assert it stays 0, including `prefer` with a
  wrong password and `protocol=modern` against 4.0 (mysql_async still sends its login there,
  which the server rejects).
* Once a TLS backend is enabled (set `TLS_BACKEND` with the mysql_async feature), `prefer`
  against a server without TLS again closes after the greeting before the clear-text retry;
  a successful retry resets the counter, a failing one does not. Such a build should peek the
  greeting's `CLIENT_SSL` flag first (`handshake::peek_greeting` + `decline`) before turning
  it on.

**Old passwords: an explicit toggle, not folded into auto.** The 3.23 scramble is weak. With
no TLS backend in this build, a man in the middle could ask for it and brute-force the
password. So BulkDB never downgrades on its own.

* `allow_old_password = true` sets `secure_auth(false)` from the first attempt. No retry is
  needed: mysql_async still answers with `mysql_native_password` unless the server switches to
  `mysql_old_password`.
* `false` (the default) maps `DriverError::MysqlOldPasswordDisabled` to `AUTH` with
  `mysql_common::OLD_PASSWORD_HINT`, which names the toggle.
* A 5.6.5+ server with `secure_auth` on (its default) refuses an old hash itself with error
  1275 ("Server is running in --secure-auth mode"): `AUTH` with
  `mysql_common::SECURE_AUTH_HINT` (the toggle cannot help; the DBA must re-hash the password).
* On a pre-4.1 server the old scheme is the only one, so the toggle does not apply.

`protocol = legacy` against a 4.1+ server gives a `CONNECT` error that names `auto`/`modern`.
`protocol = modern` against 4.0 gives whatever mysql_async reports after its login is
rejected (with `require` / `verify-full`: the `CONNECT` error above, before connecting); the
UI only offers it explicitly.

## 4. Behaviour per operation and version

Shared code works over `mysql_common::MysqlRawQuery`, which both clients implement:

```rust
trait MysqlRawQuery: Send + Sync {
    fn facts(&self) -> &ServerFacts;
    async fn meta_query(&self, sql: &str) -> Result<MetaRows>;
    async fn meta_exec(&self, sql: &str) -> Result<()>;
    async fn meta_query_cancellable(&self, sql: &str, cancel: CancellationToken) -> Result<MetaRows>;
    async fn kill_from_side(&self, target: u32) -> Result<()>;
    fn write_samples(&self) -> &WriteSamples;
}
```

`MetaRows` holds raw bytes with per-column collation ids. It is used for metadata only, never
for table data, and is capped at `META_ROW_LIMIT`.

| operation | 4.0 (legacy client) | 4.1 – 5.0 | 5.1 – 5.5 | 5.6+ / MariaDB 10+ |
|---|---|---|---|---|
| **connect** | greeting → `HandshakeResponse320` + `scramble_323` → `SHOW VARIABLES LIKE 'character_set'` → plan | mysql_async (`secure_auth` from the toggle; `max_allowed_packet`/`wait_timeout` set in `Opts` so no post-connect `SELECT @@...`); version from the greeting; `SELECT @@character_set_server, @@character_set_database, @@sql_mode`; plan; `plan.session_statements()`; `SHOW COLLATION` on `PassThrough` | same | same (today's `SET NAMES utf8mb4` becomes `plan.session_statements()`) |
| **probe** | `SHOW VARIABLES LIKE ...` one variable at a time; RAM from `innodb_buffer_pool_size`, else `key_buffer_size` | same + `SHOW GRANTS` | one `SHOW VARIABLES WHERE ...` + `SHOW GRANTS` | as today |
| probe adds | `settings.protocol` (`legacy (pre-4.1)` / `modern`), `settings.text_plan` (`TextPlan::describe`), `character_set_server`, `character_set_database`; notes from `plan_text` (latin1 hint, unsupported 4.0 charset) | | | |
| **apply_session** | `net_read_timeout`, `net_write_timeout` | same | same | + `READ ONLY` (caps.read_only_tx) + statement timeout where supported |
| **stream** | text protocol `COM_QUERY`, rows → `text_cell` → Arrow | prepared statement (binary protocol) | same | same |
| output | always `Batch::Arrow` (MySQL never produces `RawCsv`); the CSV sink formats and encodes, Parquet stores UTF-8 | | | |
| **cancel** | stop reading, drop the socket, spawned side connection `KILL <id>`; `Cancelled` immediately | side connection `KILL <id>` (no `KILL QUERY` before 5.0; the session ends) | `KILL QUERY <id>` | same |
| consumer gone | the reader races every server wait (query start, next row) against `batch_tx.closed()`: a dropped stream / `stream()` future is handled exactly like a cancel, even while the server is silent mid-scan | same (prepare, execute, next row) | same | same |
| **explain** | tabular `EXPLAIN` (`table,type,possible_keys,key,key_len,ref,rows,Extra`) → `parse_explain_tabular` | tabular (+`id,select_type`) | tabular | `FORMAT=JSON` with short timeouts (lock 3 s where `lock_wait_timeout` exists, statement 15 s where a statement timeout exists) |
| analyze | `INVALID` | `INVALID` | `INVALID` | MySQL 8.0.18+ / MariaDB 10.1+ |
| **catalog** | `SHOW DATABASES`, `SHOW TABLE STATUS FROM db` (`Type`), `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE` | `SHOW` family (`Engine` from 4.1.2, `SHOW FULL COLUMNS` with `Comment`/`Collation`) | `information_schema` (as today) | as today |
| **monitor** | `SHOW FULL PROCESSLIST`, filtered client-side; `KILL <id>` | same | `information_schema.PROCESSLIST` from 5.1.7 | + `session_connect_attrs` (5.6+) |
| **exact count** | `SELECT COUNT(*)`, cancellable; a cancel kills the session and the next use reconnects | same (`KILL <id>`) | `KILL QUERY` | as today |
| statement guard | `sqlguard` + `has_statement_separator` (4.0 lex defaults); the server rejects `;` + text | `sqlguard` + prepare (single statement) | same | + `READ ONLY` session |

Notes:

* **Columns.** Column kinds come from `mysql_common::column_kind_raw` (legacy) or the existing
  `column_kind` (modern), and both map identically. With no collation id (4.0), a BLOB type
  carrying `BINARY_FLAG` is binary and `CHAR BINARY` is text. 4.0's compact `TIMESTAMP` is
  handled by `parse_compact_timestamp` inside `text_cell`.
* **Reconnects.** After a kill (4.x cancel or exact-count cancel), both clients reconnect
  lazily on the next metadata call, using the saved parameters.
* **No server timeout below MySQL 5.7.8.** A statement timeout cannot be enforced
  server-side there. Preflight's existing checks are all the protection those servers get;
  the probe adds a note.

## 5. Text encoding on the connection (`mysql_dispatch::TextPlan`)

`plan_text(encoding, version, legacy_protocol, server_charset, database_charset)` returns a
plan and an optional note:

| server | `auto` | `utf8` | `euc-kr` | `latin1` |
|---|---|---|---|---|
| pre-4.1 | `Fixed(server character_set)`: `euc_kr` → EUC-KR, `latin1` → latin1, other → latin1 plus a note | `Fixed(UTF-8)` | `Fixed(EUC-KR)` | `Fixed(latin1)` |
| 4.1+ | `ServerUtf8` (`SET NAMES utf8mb4` from 5.5.3, else `utf8`); note when the default charset is latin1 | `ServerUtf8` | `PassThrough { client: latin1 if the DB default is latin1 else euckr, legacy: EUC-KR }` | `PassThrough { latin1, latin1 }` |

* **`PassThrough`** runs `SET character_set_client = X, character_set_connection = X,
  character_set_results = NULL`. With `results = NULL` the server sends each column's own
  collation id, so the client decodes each column by its charset: utf8/utf8mb4 → UTF-8,
  binary → bytes, euckr → CP949, everything else → the legacy charset. Collation ids map to
  charsets through `encoding::CollationMap`, filled from `SHOW COLLATION`. This is the fix for
  CP949 stored in latin1 columns: a server-side latin1→utf8 conversion would double-encode it.
* **Why `client` is latin1 for latin1 databases.** A Korean literal typed in a `WHERE`
  clause is encoded as CP949 and must compare byte-for-byte with CP949 stored in latin1
  columns, exactly as the legacy application wrote it.
* **SQL path.** The user types UTF-8 → `sqlguard` and `has_statement_separator` check the
  UTF-8 text → `encode_sql(plan.sql_charset())` → bytes on the wire (mysql_async accepts
  `&[u8]` for `query`/`prep`). Lexing UTF-8 and lexing the encoded bytes give the same
  answer, because no CP949 or windows-1252 multi-byte sequence contains `'`, `"`, `` ` ``,
  `\`, `;`, `#`, `-`, `/` or `*`. A character the target charset cannot represent is never
  silently replaced (that would change the result):
  * **Preflight blocker `SQL_ENCODING`** (`preflight::check_sql_encoding`, fed by
    `Source::sql_charset()`, which both MySQL clients report for EUC-KR / latin1 plans):
    the job is rejected before it starts, with the character and its line / column in the
    SQL the user typed. Never an acknowledgeable warning; EXPLAIN is skipped.
  * **At execution** (preflight bypassed or the plan changed): `INVALID` before sending.
    `ServerFacts::check_sql_text` runs on the SQL before the `/* bulkdb-.. */` tag or the
    `EXPLAIN` prefix is added, so the reported line / column is the user's, not shifted.
* **Metadata.** Metadata is decoded the same way (`ServerFacts::decode_meta`). One extra rule
  applies on `PassThrough{latin1, EUC-KR}`: the legacy setup stored names and comments in the
  utf8 data dictionary as windows-1252 characters, so
  `encoding::repair_latin1_mojibake` reverses that. The rule is: every character maps to
  windows-1252, at least one is non-ASCII, and the bytes decode as CP949 without error.
* **Invalid bytes.** Bytes that are invalid in the source charset become U+FFFD and are
  counted (`Decoded::replaced`). Each source logs the count once at the end of the stream, as
  a count only, never values. A `JobResult` note for decode errors is a follow-up: it needs a
  `Source::stream_notes()` default method.
* **`EUC-KR` means CP949** (WHATWG euc-kr = Windows-949, a superset), both for reading and
  writing. MySQL's own `euckr` already accepts the 8,822 extra UHC syllables.

## 6. CSV output encoding (`CsvOpts.encoding`)

* **The option.** `CsvOpts.encoding?: "utf-8" | "euc-kr"`. Absent means UTF-8, so older UIs
  and saved requests keep working. Helpers: `CsvOpts::effective_encoding()` and
  `CsvOpts::writes_bom()`.
* **BOM.** Written only with UTF-8. With EUC-KR, `bom: true` is ignored without an error,
  because Korean Excel opens CP949 natively and `--excel` stays usable. The UI disables the
  checkbox.
* **Parquet** is always UTF-8 and ignores `csv.encoding`.
* **Encoding path.** `CsvSink` places `encoding::TranscodeWriter` between its `BufWriter` and
  the `CountingWriter`, so byte counters and splitting see encoded bytes. Arrow-formatted
  cells, the header line, NULL text and RawCsv (PostgreSQL COPY) chunks all go through it.
  RawCsv chunks can split a UTF-8 sequence anywhere, so `Utf8Transcoder` carries up to 3
  bytes to the next chunk.
* **Unmappable characters: replace with `?` and count; do not fail the job.**
  * A 100M-row export should not die at row 90M because of one emoji in a free-text column,
    and rerunning costs the server another full scan. That would break the protect-the-server
    rule.
  * It is what MySQL's own charset conversion does, so users of legacy systems expect it.
  * It is never silent: the count goes to `SinkReport.unmappable_chars` (summed over split
    parts), then to a `JobResult` note via `encoding::unmappable_note`, and to a warning in
    the log.
  * A UTF-8 CSV or Parquet export is always lossless.
  * encoding_rs' built-in replacement writes HTML entities and must not be used: encode with
    `encode_from_utf8_without_replacement` and write `?` yourself.
* **Blocker.** `preflight::check_output_encoding` raises `CSV_ENCODING` when `null_as` holds
  an unrepresentable character, because NULL would become indistinguishable from `?`. It is
  never a warning, since warnings need an acknowledgement.
* **Verification recount** uses `csv::ByteRecord`, so CP949 files recount correctly. CP949
  trail bytes are never `,`, `"`, CR or LF.

## 7. Pre-4.1 protocol reference (legacy client)

Byte layouts are in the doc comments of `mysql_legacy/handshake.rs`, `packet.rs` and
`protocol.rs`. Key points:

* **Framing.** 3-byte length + sequence id. Payloads of 0xFFFFFF bytes or more are split;
  an exact multiple of 0xFFFFFF ends with an empty packet. The sequence resets per command
  and the auth packet is sequence 1.
* **Greeting.** `10`, version NUL, `int<4>` id, 8-byte scramble, `0x00`, `int<2>` caps,
  charset, `int<2>` status, 13 zero bytes. An ERR packet can arrive instead.
* **Auth (`HandshakeResponse320`).** `int<2>` flags (`LONG_PASSWORD | LONG_FLAG |
  TRANSACTIONS [| CONNECT_WITH_DB]`), `int<3>` 0xFFFFFF, user NUL, then the response NUL +
  database NUL when `CONNECT_WITH_DB` is set, otherwise the response to EOF. The user name
  and password bytes are encoded with the plan charset.
* **Scramble.** `hash_password_323` uses wrapping `u32` arithmetic and skips spaces and tabs
  (test vector: `OLD_PASSWORD('mypass') = 6f8c114b58f2ce9e`). `scramble_323` uses `my_rnd`
  with **f64**, exactly as the server does, so `check_scramble_323` matches bit for bit.
* **Packets.**
  * OK: `0x00`, lenenc affected rows, lenenc insert id, `int<2>` status when `TRANSACTIONS`
    is set.
  * ERR: `0xFF`, `int<2>` code, message. There is no SQLSTATE before 4.1.
  * EOF: `0xFE` and length < 9.
* **`ColumnDefinition320`.** Five lenenc strings: table, name, length (3 bytes), type
  (1 byte), flags (2 bytes) + decimals (1 byte) with `LONG_FLAG`. The MySQL internals
  documentation omits the length field (MySQL bug #16485).
* **Rows.** One lenenc string per column; `0xFB` is NULL.
* **TLS.** Not supported on the legacy client. `Require` / `VerifyFull` → `CONNECT` before
  connecting, with `protocol=legacy` (`precheck`) and with `auto` (`check_tls_supported`,
  section 3); `Prefer` continues in clear text, like the modern client without a TLS backend.

## 8. CLI

* `bulk-cli export ... --encoding euc-kr` (aliases `euckr` and `cp949`; default `utf-8`).
  `--bom --encoding euc-kr` is an error; `--excel --encoding euc-kr` writes no BOM.
* Connections: `--field protocol=legacy|modern|auto`, `--field encoding=euc-kr|utf8|latin1|auto`
  and `--field allow_old_password=true`, on `conn add` / `conn edit` and ad-hoc. Nothing new
  in the parser. The modern owner updates the help text.
* `bulk-cli probe` / test connection prints the new `protocol` / `text_plan` settings and
  notes.

## 9. UI touch points (done)

All paths below are under `apps/desktop/src`.

* **Connection sheet: done.** `components/dialogs/ConnectionDialog.tsx` (`DriverField`) renders
  every field with `driver.choices?.[k]` as a `Select` (value = registry value, label =
  `choiceLabel(field, value)`, falling back to the raw value; a saved value outside the list
  stays selectable as-is). PostgreSQL, which has no `choices`, keeps `SSLMODES`.
  `allow_old_password` is in `BOOL_FIELDS` and renders as a switch with its warning text
  (`hint.allow_old_password`, shown in the warning colour while on). `protocol`, `encoding`
  and `allow_old_password` (`COMPAT_FIELDS` in `ConnectionDialogForm.ts`) sit in a collapsible
  호환성 / Compatibility section of step 2. While collapsed it shows a one-line summary. It
  opens by itself when a value differs from the default. Defaults come from `driver.defaults`
  for new connections and, through `fromConnection(c, driver)`, for saved connections that
  predate the fields. An `INVALID` error from the core that names a field
  (`mysql field protocol: unknown value ...`, from `conn_test` / connect) shows inline under
  that field (`invalidFieldError`) and opens the section. A successful test of a pre-4.1
  server adds "4.1 이전 프로토콜" to the result line.
* **Strings: done.** These live in `components/dialogs/ConnectionDialog.strings.ts`
  (`defineStrings`, ko + en) instead of `lib/i18n.ts`: `field.{protocol,encoding,allow_old_password}`,
  `choice.protocol.{auto,modern,legacy}`, `choice.encoding.{auto,utf8,euc-kr,latin1}`,
  `hint.allow_old_password`, `compat`, `compatHint` and `testLegacy`. The export panel
  strings are in `components/query/strings.ts`: `encoding`, `encodingUtf8`, `encodingEucKr`,
  `encodingHint`, `encodingParquet`, `bomUtf8Only` and `unmappableNote`.
* **Export panel: done.** `components/query/OutputOptionsForm.tsx` (query tab, 출력) is the
  redesign's replacement for `extract/OutputOptionsForm.tsx`. For CSV it has an 인코딩
  segmented control (UTF-8 / EUC-KR (CP949)). With EUC-KR the BOM checkbox is disabled and
  unchecked, and a hint is shown. For Parquet the control is disabled, with the hint that
  Parquet is always UTF-8.
* **Store: done.** `stores/extract.ts`:
  * `defaultCsvOpts()` leaves `encoding` unset.
  * `setCsvEncoding` / `withCsvEncoding` drop the key for UTF-8 and clear `bom` for EUC-KR.
  * `buildRequest` normalizes the same way, so a request never carries `"utf-8"` and never
    carries a BOM with EUC-KR.
  * The Excel preset stays UTF-8 + BOM.
* **Job result: done.** `components/query/JobView.tsx` (`JobNote`, with
  `components/query/jobNotes.ts`) shows the `encoding::unmappable_note` note localized and in
  the warning colour.
* **Preflight sheet: done.** `components/query/PreflightSheet.tsx` shows `SQL_ENCODING` and
  `CSV_ENCODING` like every other blocker: the message names the character, the detail gives
  line / column, and there is no acknowledgement.
* **Mock backend: done.**
  * `lib/mock-data.ts`: the mysql driver has the registry's `fields`, `defaults` and `choices`.
    `mockLegacyConnection()` is "legacy (MySQL 4.0, EUC-KR)", tagged testing. It is listed by
    the mock backend only.
  * `lib/mock-ipc.ts`:
    * Validates the three fields like `MysqlConnOpts::from_params` (aliases accepted, an
      unknown value is `INVALID` naming the field, before connecting).
    * Probes the legacy server as `4.0.27` with `settings.protocol = legacy (pre-4.1)` and the
      `text_plan`.
    * Keeps `csv.encoding` in requests.
    * Raises `CSV_ENCODING` (NULL text outside CP949), `INVALID` (a letter delimiter with
      EUC-KR) and `SQL_ENCODING` (SQL outside CP949 on an EUC-KR connection).
    * Ignores a BOM with EUC-KR, like `CsvOpts::writes_bom`.
    * Adds the unmappable-characters note to EUC-KR results.
  * `lib/cp949.ts` holds the CP949 repertoire, built from `TextDecoder('euc-kr')`.
* **Tests: done.**
  * `components/dialogs/ConnectionDialog.compat.test.tsx`
  * `components/query/OutputEncoding.test.tsx`: encoding / BOM / Excel preset / Parquet,
    `SQL_ENCODING` in the preflight sheet, the note parser.
  * `lib/mock-ipc.compat.test.ts`: the registry parity check, the mock checks and the CP949
    table.

## 10. Test strategy

* **Unit tests (no network)**
  * Already green in the skeleton: `mysql_dispatch` (settings parsing, every `plan_text`
    row, column charsets, cache) and `mysql_common` (caps per version, `quote_str`,
    `kill_sql`, compact timestamps, legacy column kinds).
  * To add, encoding: CP949 / windows-1252 decode, `encode_sql` errors, transcoder split at
    every byte offset, the `?` count, mojibake repair.
  * To add, legacy: greeting / OK / ERR / EOF / column-def / lenenc parsing and the
    scramble vector.
* **Fake server** (`crates/core/tests/support/fake_mysql.rs`, legacy owner)
  * An in-process tokio server on `127.0.0.1:0` with the personas `Mysql40`,
    `Mysql41OldAuth` (bare `0xFE` switch) and `Mysql55PluginOldAuth` (`AuthSwitchRequest`
    for `mysql_old_password`).
  * Scripted replies with fixed, generated or endless rows.
  * It records events (connect, aborted handshake, auth, query bytes, kill) and counts
    interrupted handshakes (what a real server adds to `max_connect_errors`); it can stall
    between rows (`RowSource::Stalled`) and refuse connections with ERR 1129 / 1130 instead of
    the greeting.
  * It checks the client's auth against a stored `OLD_PASSWORD()` hash, which verifies
    `hash_password_323` independently.
  * The cases are listed at the top of `crates/core/tests/mysql_legacy_tests.rs`: connect,
    auto-detect without an aborted handshake, the old-auth toggle, EUC-KR decode, latin1
    plus euc-kr, SQL bytes on the wire, types, 16 MB continuation, bounded batches, cancel
    under 2 s, catalog, EXPLAIN, and the end-to-end export to UTF-8 and EUC-KR CSV.
* **Real MySQL 5.5** (modern owner, in `mysql_it.rs`; it runs with `--features it` when
  `BULKDB_MYSQL55_URL=mysql://root:bulkdb@127.0.0.1:3307/bulkdb` is set):
  * root has an old hash, so connecting without `allow_old_password` gives `AUTH` with the
    hint, and with it the connection works;
  * `auto` plan = utf8mb4;
  * a euckr column round-trips under `auto` and `euc-kr`;
  * a latin1 column filled with `X'C7D1B1DB'` reads "한글" under `euc-kr` and mojibake under
    `auto`;
  * a Korean comment is repaired;
  * tabular EXPLAIN, `KILL QUERY` cancel, and `apply_session` without `READ ONLY`;
  * an export to an EUC-KR CSV.
* **CI** (`.github/workflows/ci.yml`, core job; GitHub `services:` cannot pass server
  arguments, so a step starts the container):

  ```yaml
  - name: Start MySQL 5.5 (euckr, old passwords)
    run: |
      docker run -d --name mysql55 -p 3307:3306 -e MYSQL_ROOT_PASSWORD=bulkdb -e MYSQL_DATABASE=bulkdb \
        mysql:5.5 --character-set-server=euckr --collation-server=euckr_korean_ci --old-passwords=1 --secure-auth=0
      for i in $(seq 1 60); do docker exec mysql55 mysqladmin ping -uroot -pbulkdb --silent && exit 0; sleep 2; done
      docker logs mysql55; exit 1
  ```

  Then add `BULKDB_MYSQL55_URL` to the env of the `cargo test --features it` step.
* **Local** (`docker/compose.yml`): add a `mysql55` service (`image: mysql:5.5`,
  `platform: linux/amd64`, the same `command` flags, `ports: ["3307:3306"]`).

## 11. Contracts (exact signatures in the skeleton)

encoding — `crates/core/src/encoding.rs`:
`enum Charset { Utf8, EucKr, Latin1, Binary }` with `from_mysql_name(&str) -> Option<Charset>`,
`label()`, `encoding() -> Option<&'static encoding_rs::Encoding>`, `From<CsvEncoding>`;
`struct CollationMap` with `new()`, `builtin()`, `insert_named(u16, &str)`, `get(u16) -> Option<Charset>`;
`struct Decoded<'a> { text: &'a str, replaced: bool }`;
`fn decode_cell<'a>(Charset, &'a [u8], &'a mut String) -> Decoded<'a>`;
`fn encode_sql(Charset, &str) -> Result<Cow<'_, [u8]>>`;
`fn repair_latin1_mojibake(&str, Charset) -> Option<String>`;
`struct Utf8Transcoder` with `new(Charset)`, `push(&mut self, &[u8], &mut Vec<u8>) -> Result<()>`,
`finish(&mut self, &mut Vec<u8>) -> Result<()>`, `unmappable() -> u64`, `is_passthrough()`;
`struct TranscodeWriter<W: Write>` (`Write`) with `new(W, Charset)`, `unmappable()`, `finish(self) -> io::Result<(W, u64)>`;
`fn unmappable_note(u64, CsvEncoding) -> Option<String>`; `const REPLACEMENT_BYTE: u8 = b'?'`.
`types.rs`: `CsvOpts.encoding: Option<CsvEncoding>`, `CsvOpts::{effective_encoding, writes_bom}`,
`enum CsvEncoding { Utf8 = "utf-8", EucKr = "euc-kr" }`. `sink/mod.rs`: `SinkReport.unmappable_chars: u64`.
`preflight.rs`: `const CSV_ENCODING_CODE`, `fn check_output_encoding(&OutputOpts) -> Option<PreflightIssue>`;
`const SQL_ENCODING_CODE`, `fn check_sql_encoding(Charset, user_sql: &str, statement: &str) -> Option<PreflightIssue>`
(with `Source::sql_charset() -> Option<Charset>`, default `None`).
CLI: `args::EncodingArg { Utf8, EucKr }` + `to_core()`, `ExportArgs::encoding: Option<EncodingArg>`.

shared — `crates/core/src/source/mysql_dispatch.rs` (implemented):
`FIELD_PROTOCOL / FIELD_ENCODING / FIELD_ALLOW_OLD_PASSWORD`;
`enum ProtocolSetting { Auto, Modern, Legacy }`, `enum EncodingSetting { Auto, Utf8, EucKr, Latin1 }` (`parse`, `as_str`);
`struct MysqlConnOpts { protocol, encoding, allow_old_password }` + `from_params(&ConnectParams) -> Result<Self>`;
`enum TextPlan { ServerUtf8 { names }, PassThrough { client, legacy }, Fixed { charset } }` with
`sql_charset()`, `session_statements()`, `column_charset(Option<u16>, &CollationMap)`, `repairs_metadata()`, `describe()`;
`fn plan_text(EncodingSetting, &ServerVersion, bool, Option<&str>, Option<&str>) -> (TextPlan, Option<String>)`;
`is_known_modern / remember_modern / forget_protocol`; `async fn open(&ConnectParams) -> Result<Box<dyn Database>>`.
`types.rs`: `DriverInfo.choices: Option<BTreeMap<String, Vec<String>>>`; `drivers::DriverDef.choices`.

modern — `crates/core/src/source/mysql_common.rs`:
`struct MysqlCaps` + `for_version(&ServerVersion)` (implemented); `struct ServerFacts` (+ `column_charset`, `encode_sql`, `decode_meta`);
`struct MetaColumn { name, collation: Option<u16> }`, `struct MetaRows { columns, rows }` (+ `col`, `raw`, `text`, `text_by`, `u64`, `u64_by`);
`struct WriteSamples` (+ `record`, `forget`); `trait MysqlRawQuery` (above);
implemented helpers `kill_sql`, `quote_str`, `map_server_error`, `struct RawColumnDef`, `column_kind_raw`, `struct TextCell`, `text_cell`, `parse_compact_timestamp`;
to implement `probe`, `session_statements`, `apply_session`, `explain`, `parse_explain_tabular`;
`catalog::mysql::generic::{databases, schemas, tables, columns, indexes, ddl, exact_count, quote_table}`;
`monitor::mysql::generic::{snapshot, active_sessions, cancel_session}`; `impl MysqlRawQuery for MySqlDatabase` (stubbed).
Modern to-dos in `mysql.rs`:
`build_opts` gets `secure_auth(!allow_old_password)` + `max_allowed_packet` / `wait_timeout`;
`map_err` maps `MysqlOldPasswordDisabled` to `Auth(OLD_PASSWORD_HINT)`;
connect follows §4 (no unconditional `SET NAMES utf8mb4`, which fails before 5.5.3);
the stream decodes text per column on `PassThrough`;
the `Source` / `Catalog` / `Monitor` impls move to the shared functions;
`kill_query` uses `kill_sql`.

legacy — `crates/core/src/source/mysql_legacy/`:
`struct MySqlLegacyDatabase` (`connect`, `connect_with_greeting(&ConnectParams, TcpStream, Greeting)`, `version`;
`Source` / `Catalog` / `Monitor` / `Database` impls already delegate to the shared code; `MysqlRawQuery` stubbed);
`handshake::{caps, Greeting (+is_pre41, version, charset_name_40), parse_greeting, hash_password_323, scramble_323, client_flags_320, auth_packet_320, peek_greeting}`;
`packet::{PacketIo (new, reset_seq, read_payload, write_payload), MAX_PAYLOAD}`;
`protocol::{COM_*, read_lenenc_int, read_lenenc_bytes, OkPacket, ErrPacket (+into_core), is_ok, is_err, is_eof, parse_ok, parse_err, parse_column_def_320, split_text_row}`;
`conn::{LegacyConn (handshake, query, next_row, query_meta, exec, ping, quit, set_charset), QueryStart}`;
`stream::start`. Fixture: `tests/support/fake_mysql.rs` (`Persona`, `Account`, `FakeColumn`, `RowSource`, `Reply`, `Script`, `Event`, `FakeServer`).

## 12. Risks and open points

* **Detection probe.** `auto` costs one extra TCP connection per process per modern server;
  over an SSH tunnel the cache key changes every time, so it costs one per connect.
* **TLS.** Neither client has TLS in this build. Old-password auth over clear text is weak,
  which is why it sits behind an explicit toggle.
* **Mixed-charset metadata.** Metadata from a utf8 table inside a latin1 database is repaired
  only when it looks like mojibake.
* **Unsupported 4.0 charsets.** 4.0 servers with other charsets (sjis, gbk, ...) decode as
  latin1 under `auto`, with a note; an explicit `utf8` or `euc-kr` setting overrides that.
* **Decode-error counts** reach only the log. A job-result note needs `Source::stream_notes()`
  (a shared follow-up).
* **DBeaver import** (`dbeaver.rs`, shared) could map `characterEncoding=euckr` to
  `encoding=euc-kr`. This is a follow-up.
