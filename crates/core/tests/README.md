Integration tests. `cargo test -p bulk-core` runs everything that works with SQLite and the
filesystem; `cargo test -p bulk-core --features it` adds tests that need docker databases
(`BULKDB_PG_URL`, `BULKDB_MYSQL_URL`, `BULKDB_MSSQL_URL`).
