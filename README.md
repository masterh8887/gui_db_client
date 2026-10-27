# BulkDB

Large result sets, straight to files. A Windows/macOS desktop app that streams millions of
rows from PostgreSQL, MySQL/MariaDB, SQL Server, SQLite (and Oracle/ODBC via vendor clients)
into CSV or Parquet at `psql \copy` speed, with fixed memory, SSH tunnels, server-protecting
preflight checks, progress/ETA and cancellation. It shows metadata, never data.

- Core engine: Rust (`crates/core`), also usable headless (`crates/cli`).
- Shell: Tauri 2 + Next.js 16 static export (`apps/desktop`).
- See `CLAUDE.md` for the architecture rules and the full command list.

```bash
pnpm install
pnpm typegen
pnpm --filter desktop dev      # opens the app
cargo run -p bulk-cli -- --help
```
