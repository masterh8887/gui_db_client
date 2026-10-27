## What

<!-- one paragraph -->

## Non-goals check (v1 — reviewers reject PRs that cross these)

- [ ] No table **data** is shown anywhere (no preview, no result grid, no row editing).
- [ ] No data rows cross IPC; the UI only receives metadata, progress and events.
- [ ] No `Vec<Row>` / `collect()` of a result set; bytes stream `Source → channel → Sink`.
- [ ] No business logic in `src-tauri`; no `tauri::` types in `crates/core`.
- [ ] No `#[cfg(target_os)]` outside `crates/core/src/platform/`.
- [ ] No secret in config files, logs, history or IPC responses (keyring only).
- [ ] Extraction SQL stays a single `SELECT`/CTE; session settings only via the allow-list.

## Verification

- [ ] `cargo test -p bulk-core` and `pnpm --filter desktop lint && pnpm --filter desktop test` pass
- [ ] `pnpm typegen:check` is clean (IPC types regenerated and committed)
- [ ] If the core changed: wide bench within 5 % of main, memory gate reported
