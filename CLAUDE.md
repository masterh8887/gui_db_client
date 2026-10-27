# BulkDB — large result sets, straight to files

Desktop app (Windows/macOS) that streams millions of rows from a database into CSV/Parquet at
`psql \copy` speed. "A DB client minus the data grid": metadata (databases, schemas, tables with
row estimates and sizes, columns, indexes, DDL) is shown; table data is **never** shown, only
written to files. Spec: `BulkDB — 대용량 DB 추출 데스크탑 앱 개발 사양서.pdf` (34 pages; the
summary below is binding).

## Layout

```
crates/core/        bulk-core  — ALL logic (no Tauri types here, ever)
crates/cli/         bulk-cli   — headless runner over bulk-core
apps/desktop/       Next.js 16 (App Router, output: 'export') + Tauri 2 shell (src-tauri/)
packages/ipc-types/ TS types generated from Rust by ts-rs (`pnpm typegen`), never hand-written
drivers/registry.json  driver definitions (tiers, fields, vendor-library probes)
```

Core modules: `types.rs` (IPC payloads), `error.rs` (`CoreError` → `{code,message,detail?}`),
`source/` (Source trait + one driver per file), `sink/` (csv, parquet, compress, split),
`catalog/`, `monitor/`, `job.rs` (pipeline + 9-state job machine), `preflight.rs`,
`sqlguard.rs`, `tunnel.rs` (OS ssh under a PTY), `drivers/`, `credentials.rs` (keyring),
`config.rs`, `history.rs` (sqlite), `logging.rs` (masking), `platform/` (the ONLY place with
`#[cfg(target_os)]`), `service.rs` (`BulkDb` facade used by Tauri commands and the CLI).

## Five rules (reviewers revert violations)

1. **No row objects.** Bytes flow `Source → bounded channel → Sink`. Never `Vec<Row>`, never
   `collect()` a result. PostgreSQL CSV = `COPY TO STDOUT` bytes passed through untouched.
2. **Fixed memory.** Channel depth 8, batch ≤ 64K rows / 8 MB. 100M rows must stay < 100 MB RSS.
3. **Core ≠ UI.** `src-tauri/commands.rs` functions are ~10 lines: convert args, call core,
   return. No `tauri::State` in `crates/core`. The CLI must do everything the UI does.
4. **Three driver tiers.** Pure-Rust drivers built in (PG, MySQL, MSSQL, SQLite); vendor
   clients (Oracle, ODBC) detected at runtime; never shell out to `psql`/`bcp`.
5. **One code base for Windows and macOS.** Platform branches only in `crates/core/src/platform/`.

Also: protect the server — preflight checks `work_mem` vs RAM, concurrent sessions, dangerous
plans; warnings must be acknowledged, blockers reject. Secrets only in the OS keyring, never in
config/logs/IPC. Extraction SQL is a single `SELECT`/CTE; DML/DDL rejected. Data rows never
cross IPC. No data preview anywhere. Row counts are estimates unless the user clicks "count".

## Commands

```bash
source ~/.msvc/env.sh                 # this PC only: portable MSVC + cargo on PATH (see CLAUDE.local.md)
cargo check -p bulk-core              # fast compile check
cargo test -p bulk-core               # unit + SQLite integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                       # rustfmt.toml: max_width 170 (CI runs --check)
cargo run -p bulk-cli -- export --driver sqlite --path test.db --query "select 1" --out out.csv --progress
pnpm install                          # pnpm 12.14.0 exactly (engine-strict)
pnpm typegen                          # regenerate packages/ipc-types from Rust
pnpm --filter desktop lint            # eslint + tsc
pnpm --filter desktop test            # vitest
pnpm --filter desktop build:web       # next export → apps/desktop/out
pnpm --filter desktop dev             # tauri dev (opens the window)
```

Run cargo from the repo root. The Cargo target dir is shared; concurrent cargo invocations
queue on the build lock (that is fine).

## Conventions

- Rust edition 2021, `thiserror` in core, `anyhow` only in the CLI. All IPC types derive
  `Serialize, Deserialize, TS` with `#[serde(rename_all = "camelCase")]`. Every optional
  field carries both `#[ts(optional)]` and `#[serde(skip_serializing_if = "Option::is_none")]`:
  the TS type says `field?: T`, so the wire must omit the key, never send `null`.
- Errors to the UI are only `ErrorPayload { code, message, detail? }`; codes are
  `CoreError::code()` values (`CONNECT, AUTH, QUERY, IO, CANCELLED, TIMEOUT, DRIVER_MISSING,
  OOM_RISK, INVALID, TUNNEL, NOT_FOUND, INTERNAL`).
- Tests: unit tests next to the code; SQLite-backed pipeline tests in `crates/core/tests/`;
  docker-backed tests behind `--features it` and `BULKDB_PG_URL` etc.
- TypeScript: `tsc` is TypeScript 7.0.2 (the native compiler, installed as the
  `@typescript/native` alias, pinned by the spec). TS 7 ships no JavaScript API, so the
  `typescript` package name resolves to `@typescript/typescript6` (the 6.0 API) for
  typescript-eslint, exactly as Microsoft's TS 7 release notes recommend. Do not "fix" this.
- pnpm 12 blocks install scripts unless listed under `allowBuilds` in `pnpm-workspace.yaml`.
- Frontend: TypeScript strict, zustand stores in `src/stores`, components in
  `src/components`, IPC only through `src/lib/ipc.ts`. 13px font, 28px rows. State of record is
  Rust: the UI renders `job:*` events and never changes job state on its own.
- Keep files you do not own untouched when working in parallel with other agents; add new
  modules rather than editing shared ones, and never run `pnpm add` without coordination
  (the lockfile is shared).
