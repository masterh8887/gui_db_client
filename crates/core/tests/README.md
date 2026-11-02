Integration tests. `cargo test -p bulk-core` runs everything that works with SQLite and the
filesystem; `cargo test -p bulk-core --features it` adds tests that need docker databases
(`BULKDB_PG_URL`, `BULKDB_MYSQL_URL`, `BULKDB_MSSQL_URL`, and the DB copy servers below).

A test whose environment variable is missing prints a skip note and passes, so every file always
compiles and never fails on a machine without docker (`cargo check -p bulk-core --features it
--tests`). `docker compose -f docker/compose.yml up -d --wait` starts every server below except
MySQL 4.0; CI (`.github/workflows/ci.yml`) sets the same variables.

## Without docker

| file | what |
|---|---|
| `copy_tests.rs` | DB copy end to end (docs/db-copy.md section 9): SQLite -> SQLite through the public `BulkDb::copy_*` facade (policies, cancel + resume, dry run, refusals, `TARGET_IN_USE`, RSS bound), the in-process fake MySQL 4.0 EUC-KR server (`support/fake_mysql.rs`) as a source into a `RecordingTarget`, and the real `MysqlTarget` against the fake 5.5 server (INSERT path, committed rows after a failed chunk). `BULKDB_COPY_RSS_CHILD` is set internally for the RSS child process. |
| `mysql_legacy_tests.rs` | the pre-4.1 client against the fake 4.0 server. |
| `pipeline_tests.rs`, `sink_tests.rs`, `sqlite_tests.rs`, `service_sqlite_tests.rs`, `sqlguard_tests.rs` | export pipeline, sinks, SQLite source, facade, SQL guard. |
| `tunnel_tests.rs` | SSH tunnels with a fake `ssh` script; a real bastion only with `BULKDB_SSH_URL` (+ `BULKDB_SSH_TARGET`, `BULKDB_SSH_KEY`, `BULKDB_SSH_KEY_PASSPHRASE`). |
| `odbc_tests.rs` | ODBC driver detection. |

## With docker (`--features it`)

| variable | server (docker/compose.yml) | used by |
|---|---|---|
| `BULKDB_PG_URL` | `postgres:15`, port 5432 | `postgres_it.rs`; DB copy source and same-version target in `copy_it.rs` |
| `BULKDB_PG16_URL` | `postgres:16`, port 5433 | `copy_it.rs`: pg15 -> pg16 (full database, cancel + resume) |
| `BULKDB_PG17_URL` | `postgres:17`, port 5434 | `copy_it.rs`: pg15 -> pg17 (full database, cancel + resume, `PG17_SEARCH_PATH`) |
| `BULKDB_MYSQL_URL` | `mysql:8`, port 3306 | `mysql_it.rs`; DB copy target in `copy_it.rs` |
| `BULKDB_MYSQL55_URL` | `mysql:5.5` with `--character-set-server=euckr`, port 3307 | `mysql_it.rs` (legacy server); DB copy EUC-KR source in `copy_it.rs` |
| `BULKDB_MYSQL9_URL` | `mysql:9` (root uses `caching_sha2_password`; 9.x has no `mysql_native_password`), port 3308 | `copy_it.rs`: mysql 5.5 euckr -> mysql 9 (utf8mb4) |
| `BULKDB_MYSQL40_URL` (optional) | a MySQL 4.0 server built from the 4.0.27 binaries (no public image exists), e.g. port 3309 | `copy_it.rs`: pins the 4.0 source reads (`UNIX_TIMESTAMP` for `timestamp(N)`, `c + 0e0` for FLOAT, `c + 0` for YEAR(2)) against real output; needs `BULKDB_MYSQL_URL` as the target |
| `BULKDB_MSSQL_URL` | SQL Server 2022, port 1433 | `mssql_it.rs` |

URLs are `postgres://user:password@host:port/database` and `mysql://user:password@host:port/database`
(percent-encoded password).

### `copy_it.rs` (DB copy against real servers)

- mysql 5.5 euckr -> mysql 8 and -> mysql 9: Korean text, ENUM, zero and invalid dates, TIMESTAMP,
  YEAR, DECIMAL, BLOB, BIT; both load methods (LOAD DATA LOCAL and INSERT); a blob over
  `max_allowed_packet` fails with `ROW_TOO_LARGE`; a truncating value fails with `DATA_WARNINGS`.
- pg15 -> pg16 and -> pg17: the whole database (identity, serial, enum, domain, partitions,
  inheritance, views, a materialized view, functions, triggers, a deferrable FK, NOT VALID checks,
  comments, `citext`, RLS) with a forced cancel and `copy_resume`.
- pg15 -> pg15 into a second database: dry run, append, RLS.
- MySQL 4.0 (only with `BULKDB_MYSQL40_URL`).

Isolation: every test creates its own source and target databases (`bulkdb_it_src_<uuid>`,
`bulkdb_it_<uuid>`) and drops them at the end. Tests that change MySQL server globals
(`local_infile`, `max_allowed_packet`, as test setup only) hold a static per-server mutex, so the
copies touching one MySQL server run one at a time.

```text
docker compose -f docker/compose.yml up -d --wait
BULKDB_PG_URL=postgres://bulkdb:bulkdb@127.0.0.1:5432/bulkdb \
BULKDB_PG16_URL=postgres://bulkdb:bulkdb@127.0.0.1:5433/bulkdb \
BULKDB_PG17_URL=postgres://bulkdb:bulkdb@127.0.0.1:5434/bulkdb \
BULKDB_MYSQL_URL=mysql://root:bulkdb@127.0.0.1:3306/bulkdb \
BULKDB_MYSQL55_URL=mysql://root:bulkdb@127.0.0.1:3307/bulkdb \
BULKDB_MYSQL9_URL=mysql://root:bulkdb@127.0.0.1:3308/bulkdb \
  cargo test -p bulk-core --features it --test copy_it
```
