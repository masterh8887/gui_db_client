# DB copy, several windows and several instances (binding spec)

Status: approved by the user, to be implemented. Request, in the user's words: "pc 어플리케이션은 N개의 멀티도 프로그램을 킬수 있어야해. 그리고 멀티 또는 여러개의 커넥션이 연결될수 잇는 manage가 있을거고, 특정 데이터베이스의 모든 schema + table + data 를 다른 데이터베이스에 복사 또는 데이터 밀어넣기를 할수 있는 최고의 기능이 필요해. 대신 mysql4 euc-kr -> mysql5,mysql6, mysql7, mysql8, msyql9 utf-8 / postgresql15 -> postgresql16, postgresql17 등 자신의 db 유형은 같아야 하는 조건이 있을수 있지만 character-set이 다른 부분을 설정하는 것을 통해 바뀌게 밀어넣고 할수 있는 기능이어야하는거야."

What changes:

1. **Windows and instances.** One process shows several windows; several BulkDB processes may run at once (section 1).
2. **Connection manager.** Every window keeps several open connections (as today). Welcome also shows which connection is open in which window and focuses that window.
3. **DB copy.** Copy the schemas, tables (PostgreSQL: also its other schema objects) and data of one database into another database of the **same family**, converting character sets on the way. Required paths:
   - MySQL 4.x EUC-KR, read through the legacy pre-4.1 client, into MySQL 5.0-5.7, 8.0, 8.4, 9.x; the target uses utf8mb4 wherever the server supports it.
   - PostgreSQL 15 into 16 or 17, and copies between servers of the same version.
   - SQLite into SQLite (local tests).

   MySQL 6.x and 7.x never shipped as GA: 6.0 was a cancelled alpha, 7.x is NDB Cluster numbering (an NDB 7.x server reports a 5.x version, e.g. `5.7.44-ndb-7.6.29`, and gets that version's rules). This is a caption in the MySQL facts line of the plan and in the CLI plan header, and a notice only when the server version contains `-ndb-`.

BulkDB was read-only by design. Copy is the only writer, and only into an explicitly write-allowed **target** (section 8). CLAUDE.md still applies in full: no row objects, fixed memory, cancel in about 2 s, secrets only in the keyring, the CLI does everything the UI does, `cfg(target_os)` only in `platform/`, no new dependencies (crates already in Cargo.lock only, never `pnpm add`), optional IPC fields carry `#[ts(optional)]` + `skip_serializing_if`, Korean strings with English fallbacks via `defineStrings`. No data preview: besides the copy stream, core reads values only for the bounded charset sample of 3.2, which never leaves the planner.

**Non-goals (v1):** copies across families (MSSQL, Oracle, ODBC, PG↔MySQL); copies inside one database (schema to schema; later, and only into a missing or empty schema); MySQL stored routines and events; PG rules, aggregates, operators, operator classes and families, casts, text search configurations and dictionaries, statistics objects, FDW objects, publications, subscriptions, event triggers, large objects, tablespaces, typed tables, database/role settings (all listed `unsupported` or reported by an issue, never dropped silently); binary COPY; creating PostgreSQL databases (the PG target database must exist; a missing MySQL target database is created); a consistent snapshot across MyISAM tables or across several MySQL workers (`SRC_NOT_SNAPSHOT`, 4.1).

## 1. Several windows and several instances

### 1.1 Windows (one process)

**Labels and creation.**
- `main` comes from `tauri.conf.json`. Other windows are `w-<n>` (n counts up, never reused).
- `window_new(connId?)` is an **async** command (creating a window from a synchronous command deadlocks on Windows). It uses `WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))` with the window options of `main` (1280x820, min 1024x680, title "BulkDB"), cascaded +24 px from the focused window.
- At most **8 windows** per process; the 9th call fails `INVALID` "창은 최대 8개까지 열 수 있습니다".
- Capability `default.json`: `"windows": ["main", "w-*"]`, plus `core:window:allow-set-focus`, `core:window:allow-close`, `core:window:allow-destroy` (the JS `onCloseRequested` destroys the window when not prevented).

**Registry** (`src-tauri/src/windows.rs`). `WindowRegistry(Mutex<RegistryState>)` with `RegistryState { entries: BTreeMap<label, WindowEntry>, focus_order: Vec<label> /*most recent first*/, pending_init: HashMap<label, connId>, next_n: u32, prompts: Vec<PendingPrompt>, job_windows: HashMap<jobId, label> }` and `WindowEntry { label, index: u32 /*1-based display number, smallest free*/, conn_id?, conn_name?, title }`. Updated by `WindowEvent::Focused`, `WindowEvent::Destroyed`, `window_new`, `window_set_connection`, `window_claim_job` and `connections:changed`. After every change the shell emits `windows:changed` (`WindowInfo[]`) to **all** windows. The pure parts (index allocation, titles, prompt and job hand-over targets) are plain functions with unit tests.

**Titles.** Welcome: "BulkDB". Open workspace: `BulkDB — <connection name>` (em dash). Set by the shell in `window_set_connection` and when a rename arrives with `connections:changed`. The frontend calls `window_set_connection(openConnId | null)` from `stores/windows.ts`, which subscribes to `workspace.mode` / `workspace.openConnId`.

**Start of a window.** `AppShell` calls `initWindows()` once; it calls `window_init()` → `WindowInit { label, index, openConnId? }` and, with `openConnId`, runs `workspace.openWorkspace(openConnId)` (password prompts as today, in that window). Windows opened without a connection start on Welcome.

**Events and routing.**
- `job:*`, `drivers:changed`, `copy:progress`, `connections:changed`, `settings:changed`, `windows:changed`, `prompt:resolved` go to every window (`app.emit`).
- **Window-targeted events** (`menu:action`, `tunnel:hostkey`, `tunnel:password`, `jobs:claimed`, `app:quit-requested`) are sent with `emit_to(<label>)`. Tauri 2.12 also delivers an `emit_to` to listeners registered with the default target `{ kind: 'Any' }`, so the frontend subscribes to them only through `listenWindow(name, cb)` in `lib/ipc.ts`: `listen(name, cb, { target: { kind: 'WebviewWindow', label: getCurrentWebviewWindow().label } })`; in mock mode a per-window bus. AppEvents uses it for all five; a vitest asserts the target option.
- **Menu actions** (macOS menu bar) go only to the focused window; today `menu.rs` uses `app.emit`, which would run an action in every window. The custom Close Window (⇧⌘W) closes the **focused** window, not `main`.
- **Tunnel prompts** go to the focused window (else the most recently focused). `HostKeyPrompt` and `SshPasswordPrompt` gain `opt conn_name` and `opt job_label`; HostKeyDialog and SshPasswordDialog show "{connection} · {job}", so a prompt raised by a copy started in another window has context. Core fills them: `BulkDb::open_connection` resolves the job label from the session's application name (`bulkdb-<job id>` or `bulkdb-<job id>-<session>`) into `ConnectRequest.job_label`, and `connect::open_connection` passes the connection name and that label as a `tunnel::PromptContext` to `TunnelManager::acquire_with` (a reused tunnel prompts nothing). The registry keeps each pending prompt until core emits `prompt:resolved { promptId }`. `prompts.rs` emits `PromptResolved` on **every** removal (answer, `PROMPT_TIMEOUT`, forget, and the drop guard of a cancelled waiter), not `service.rs`; unit test: a timeout emits it. When the window holding a prompt closes, the prompt is re-sent to the next window in `focus_order`.
- DB password prompts stay window-local UI.

**Per-window state.**
- zustand stores live per webview: workspace, tabs, docs, catalog, ui, palette, copy wizard. Two windows may open the same connection; each has its own tabs.
- The persisted ui slice (localStorage) is shared by the windows of one process: `stores/windows.ts` listens to the `storage` event for the ui key and calls `useUiStore.persist.rehydrate()`, so theme and language follow across windows. Across processes this is not reliable (separate WebView2 environments, WKWebView): per-process divergence of theme and language is accepted; a process reads the stored values when it starts.
- Session (typed) passwords live in core: shared by every window of the process.

**Jobs are process-global.** Every window's jobs store receives every job; Activity shows all. `stores/jobs.ts` gains `local: Record<string, true>` and `markLocal(jobId)`; export, count and copy starts call it, and it calls `window_claim_job(jobId)`. When a window is destroyed, the registry hands its active claims to the next window in `focus_order` and sends it `jobs:claimed { jobIds }`, which adds them to `local`. Completion toasts show in the window holding the claim, so they are never lost with a closed window. With more than one window, a non-local job row carries the caption "다른 창에서 시작". When a copy job ends, core invalidates its catalog cache for the target connection, and every window's AppEvents calls `catalog.invalidate(summary.copy.targetConnId)` on that `job:done`.

**Closing.** A window with dirty query docs asks first (as `closeWorkspace` does) through `getCurrentWindow().onCloseRequested`. Closing the **last** window while jobs are active asks "실행 중인 작업 {n}개를 취소하고 종료할까요?" (decline keeps it open). On `RunEvent::ExitRequested` (Cmd+Q, menu Quit, OS) with active jobs and no confirmation yet, the shell calls `api.prevent_exit()` and sends `app:quit-requested { jobs }` to the focused window, which asks the same question; accept → `app_quit_confirmed` (cancels the jobs, sets the confirmation flag, exits). `state::shutdown` runs on the confirmed exit or when the last window is destroyed.

**Shortcut and actions.**

| keys | action id | scope |
|---|---|---|
| `Mod+Shift+N` | `window.new`: empty Welcome window (menu: File → 새 창 ⇧⌘N) | always, `menuOwned` |
| — | `window.openConnection`: "새 창에서 열기" for the selected Welcome row (Welcome-local `Mod+Enter`) or a palette connection row (palette-local `Mod+Enter`) | always |
| — | `app.newInstance`: "새 BulkDB 실행" (menu: File → 새 BulkDB 실행), section 1.2 | always |
| — | `copy.open`: "데이터베이스 복사…" (menu: File → 데이터베이스 복사…, no accelerator) | always |

`Mod+Shift+N` is free in DESIGN.md §6; in mock/browser mode it belongs to the browser, like `Mod+T/W/N`. `menu.rs` gains `window.new`, `app.newInstance` and `copy.open`.

**Connection manager (Welcome), toolbar and status pill** (owner **windows**, calling `useCopyStore.getState().open(preset)`).
- Rows keep "열림 · 탭 N" for this window. Each **other** window with the connection open adds a chip "창 {index}"; clicking it calls `window_focus(label)`.
- Row menu gains 새 창에서 열기, **이 연결을 원본으로 복사…** / **이 연결을 대상으로 복사…** (section 7) and, for write-allowed connections, 쓰기 허용 해제 (`conn_save` with `writeAllowed: false`).
- Write-allowed rows show a 12px `PenLine` icon, tooltip "쓰기 허용 (DB 복사 대상)".
- A group "열린 연결" (connections open in any window) is listed first when `welcomeGroupBy` is `none` and any connection is open.
- Workspace toolbar connection-switcher menu gains 새 창 (`window.new`) and 데이터베이스 복사… (`copy.open`): the workspace entry point on Windows, which has no menu bar.
- The workspace status pill popover lists active copy jobs whose target is this connection: "이 연결로 복사 중 · {label}".

### 1.2 Several processes

There is no single-instance plugin and none is added.

**Per process:** window registry; jobs and the job limit (`max_concurrent_jobs` counts per process); SSH tunnels; catalog cache and open connections; session passwords. **Shared through files:** `connections.toml`, `settings.toml`, `history.sqlite` (with copy checkpoints and run claims), the OS keyring, the target leases of 5.1.

| what | rule (owner **windows**) |
|---|---|
| config writes (`config.rs`) | Every mutation (upsert, delete, set_settings, import) runs in `with_lock`: open `<config_dir>/.config.lock` and call `fs4::fs_std::FileExt::try_lock_exclusive(&file)` (fully qualified: std has inherent `lock*` methods; returns `Ok(false)` while another process holds it). Poll every 25 ms for up to 5 s, then `IO` "다른 BulkDB가 설정을 저장하는 중입니다". Inside the lock: re-read the file, apply only this call's delta (connections by id; settings as a whole object, last writer wins), `write_atomic`, update the cache, record the stamp (mtime + len), unlock. The `write_allowed` rules of section 2 are resolved inside `ConfigStore::upsert`, under the lock, against the re-read file. |
| config reads | `ConfigStore::reload_if_changed() -> ConfigChange { connections: Vec<String> /*changed or removed ids*/, settings: bool }` compares stamps and reloads. `BulkDb::config_poll()` calls it; for changed settings it resizes the export slots exactly as `settings_set` does and emits `SettingsChanged(Settings)`; for changed connections it invalidates their catalog and browser caches and emits `ConnectionsChanged`. The shell calls it every 2 s. `conn_save`, `conn_delete`, `settings_set` emit the same events for the other windows of the process. The frontend reloads `connections.load()` / settings on them. |
| `history.sqlite` | WAL (already) + `PRAGMA busy_timeout = 5000` on every connection (`History::open` and the copy checkpoint store), both opened through `history::open_shared` (busy_timeout before switching to WAL) with their schema setup in `history::retry_busy`. |
| logs (`logging.rs`) | One file per process: `tracing_appender::rolling::Builder::new().rotation(Rotation::DAILY).filename_prefix(format!("bulkdb-{pid}")).filename_suffix("log")` → `bulkdb-<pid>.<date>.log`. `init` deletes `bulkdb*.log` files older than 14 days. Diagnostics keeps taking the newest two files. |
| instances (`instances.rs`, new) | `register(config_dir, kind) -> InstanceGuard` runs in `BulkDb::new` when `BulkDbOptions.instance_kind` is set (desktop `"desktop"`, CLI `"cli"`, tests `None`). It first creates and exclusively locks `<config_dir>/instances/<pid>.lock` (a separate file: on Windows a locked range cannot be read), then writes `<pid>.json` `{pid, instanceId /*uuid v4*/, kind, startedAt, version}`; after locking, the owner re-checks that its `.lock` file still exists and recreates it if a scanner deleted it in between. `info()` lists the others and skips its own pid: a `.lock` it can lock is stale (both files deleted); one it cannot lock is live. `is_alive(instance_id)` reads the json of that id's pid and requires the same `instanceId` (a reused pid is not alive); the own instance is alive without any file test, and a json naming this process's pid with an id it never registered is a dead predecessor (not alive). A crash between locking and writing the json (or between the two removals of an exit) leaves a `.lock` without its json: `info()` removes it once it is older than 10 s and nobody holds it (a registration in progress holds its lock at once; `register` retries a lock a scanner holds for a moment). Removals happen while holding the lock. |

**Starting another process.** On Windows, launching BulkDB again starts a second process. On macOS, LaunchServices focuses the running instance instead, so File → 새 BulkDB 실행 (`app.newInstance` → command `instance_spawn`) starts one through `platform::spawn_instance()`: macOS `open -n <bundle path>`, Windows `current_exe()` spawned detached. The pure `platform::instance_command(exe) -> (program, args)` is unit-tested. `open -n -a BulkDB` and the CLI work too; the macOS menu item's tooltip says so.

**How the UI explains it.** Welcome's header shows a quiet chip "다른 BulkDB {n}개 실행 중" when other live instances exist, tooltip "작업, 동시 작업 수 제한, SSH 터널, 입력한 비밀번호는 프로그램(프로세스)마다 따로입니다. 연결, 설정, 기록은 함께 씁니다." The workspace status popover shows the same chip. Activity's footer says "이 BulkDB 프로그램의 작업만 표시됩니다" when others exist.

## 2. Copy concepts and IPC types

- Source and target connections must have the same driver: `postgres`, `mysql` or `sqlite`; otherwise blocker `FAMILY_MISMATCH` / `FAMILY_UNSUPPORTED`. Saved connections only (UI and CLI).
- A copy is a **job** of the new kind `JobKind::Copy` (wire `"copy"`) on the nine-state machine. It takes one export slot while `running` but opens several sessions on each server; `CopyEffective.source_sessions` / `target_sessions` give the totals (4.1), shown in the plan facts line and the concurrency help text.
- A copy job belongs to a **run** (`runId` = the first job's id), checkpointed in `history.sqlite`. A resume is a new job in the same run.

**Issue conventions** (plan issues, all codes in this spec):
- Severity is the existing `Severity` (`info`, `warning`, `danger`). "**blocker**" means the issue is in `CopyPlan.blockers` (its severity is `danger`); any blocker rejects the job.
- "**item blocker**" = a blocker with `item_id` set. It is resolved by excluding that item (`CopyOptions.exclude`, UI button 제외) or by changing an option.
- `message` is English (as in export preflight). The UI titles known codes through `copy.issue.<CODE>` strings and falls back to `message`.

**Acknowledgement.** Core lists what needs one in `CopyPlan.acks: Vec<AckNeed>`: one entry per distinct `warning`/`danger` code in `plan.warnings` and `items[].warnings`, with `items` = the sorted ids of the items carrying it. `key` = the code when no item carries it, else `<CODE>#<first 12 hex digits of SHA-256 over the item ids joined by "\n">`. `copy_start`, `copy_resume` and `job_resume` take keys, and every `acks[].key` must be among them. A different set of affected items is a different key, so an old acknowledgement never covers new tables. `info` never needs one. `PRODUCTION_CONFIRM` is never acknowledged: typing the name satisfies it.

**Plan validity.**
- `plan_id` = the first 32 hex digits of SHA-256 (`sha2`, in Cargo.lock) over a canonical JSON of: family; both endpoints' server identity key (5.3), database and schema; `effective`; every item's `(id, kind, action, phase, order, target_sql)`, where finalize items contribute their template (see `PlanItem.target_sql`); and the codes of translation and item issues. Not hashed: estimates, `target_has_rows`, `acks`, `session_sql` values that echo server variables, and the issues marked **env** in 5.3 (they follow live state: row presence, sizes, sessions, free space, other runs, the production name). The hash is stable across processes, so `bulk-cli copy run --expect-plan` can bind to a reviewed plan.
- **Start-time fields.** `options.dry_run` and `options.confirm_production` never change a plan and are not hashed. The wizard keeps them outside `draft` and merges them only in `start()` / `resume()`. Any other draft change after planning invalidates the plan and sends the user back to [계획 만들기].
- Unit test: the same request planned by `copy_plan` and again inside `copy_start` with a correct `confirm_production` gives the same `plan_id`.

**Listing conventions:** `opt` = `#[serde(default, skip_serializing_if = "Option::is_none")] #[ts(optional)]`. Structs derive `Debug, Clone, Serialize, Deserialize, TS, PartialEq`, `rename_all = "camelCase"`. Enums are `rename_all = "snake_case"`, with `Default` on the variant marked `def`. Option structs carry struct-level `#[serde(default)]` with a manual `impl Default` holding the values in the comments; TS sends whole objects (`DEFAULT_COPY_OPTIONS`).

The types live in `crates/core/src/copy/types.rs`; `types.rs` does `pub use crate::copy::types::*;` and `ts_export.rs` exports every IPC type below.

```rust
pub enum CopyFamily { Postgres, Mysql, Sqlite }
pub struct CopyEndpoint {
    pub conn_id: String,
    pub database: String,            // MySQL database; PG database (the session opens it: conn.database overridden); SQLite "main"
    opt pub schema: Option<String>,  // PG only. Source: restrict to one schema (absent = every user schema).
                                     // Target: only with a source schema; that schema is written into this one (a rename into another database).
    opt pub password: Option<String>,// "ask every time" connections; never stored, never in history
}
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CopySelection { All, Tables { tables: Vec<TableRef> } }  // TS {kind:"all"} | {kind:"tables", tables: TableRef[]}
// TableRef.schema: PG schema (required), absent for MySQL/SQLite; TableRef.database is ignored.
pub enum CopyContent { Both /*def*/, SchemaOnly, DataOnly }
pub enum ExistingTablePolicy { Skip /*def, USER DECISION*/, Truncate, DropRecreate, Append }
pub struct ItemPolicy { pub item_id: String, pub policy: ExistingTablePolicy }     // per existing table (3.7)
pub enum VerifyMode { None, RowCount /*def*/, Checksum }   pub enum ZeroDatePolicy { Auto /*def*/, Null, Keep, Min }
pub enum UnmappablePolicy { Replace /*def: replace and count; the table ends "done with problems"*/, Fail }
pub enum EnumErrorPolicy { Fail /*def: table fails with ENUM_ERROR_VALUE*/, Null /*load '' as NULL, counted as ENUM_ERROR_NULL*/ }
pub enum MysqlEngineChoice { Auto /*def*/, Keep, Innodb, Myisam }   pub enum ColumnTreatment { Binary, Text }
pub enum PgSourceEncoding { EucKr, Latin1, Utf8 }   pub enum NameCase { Keep /*def*/, Lower }   pub enum RenameKind { Schema, Table }
pub struct NameMapping { pub kind: RenameKind, opt pub schema: Option<String> /*Table in PG: its source schema*/, pub from: String, pub to: String }
pub struct CopyObjectFlags {        // defaults: all true except owners, grants
    pub views: bool, pub routines: bool /*PG functions + procedures*/, pub triggers: bool,
    pub sequences: bool /*only the SequenceValue / AutoIncrement items (3.7)*/,
    pub types: bool /*PG*/, pub extensions: bool /*PG*/, pub comments: bool, pub owners: bool /*false, PG*/,
    pub grants: bool /*false, PG*/, pub analyze: bool, pub refresh_matviews: bool /*PG*/,
}
pub struct MysqlTableOverride { pub table: String /*source table*/, opt pub charset: Option<String>, opt pub collation: Option<String> }
pub struct MysqlColumnOverride { pub table: String, pub column: String, pub treat_as: ColumnTreatment }
pub struct MysqlCopyOptions {
    opt pub charset: Option<String>,       // absent = version default (3.3)
    opt pub collation: Option<String>,     // absent = the charset's default on that target; "legacy" = PAD SPACE preset (3.3)
    pub engine: MysqlEngineChoice,
    pub zero_dates: ZeroDatePolicy,
    opt pub time_zone: Option<String>,     // "+09:00"; used only by the DATE_FORMAT fallback of 3.2
    pub fix_row_size: bool,                // true (3.3 row limits)
    pub keep_definer: bool,                // false (3.3 views and triggers)
    opt pub enum_error: Option<EnumErrorPolicy>, // absent = Fail (3.3 ENUM error value)
    pub table_overrides: Vec<MysqlTableOverride>,
    pub columns: Vec<MysqlColumnOverride>,
}
pub struct PgCopyOptions {
    pub collation_fallback: bool,      // false: missing collation = item blocker; true: database default + warning
    opt pub source_encoding: Option<PgSourceEncoding>, // SQL_ASCII sources only (3.5)
}
pub struct CopyOptions {
    pub content: CopyContent, pub existing: ExistingTablePolicy, pub existing_overrides: Vec<ItemPolicy>,
    pub objects: CopyObjectFlags, pub name_case: NameCase, pub rename: Vec<NameMapping>,
    pub exclude: Vec<String>,          // plan item ids left out (their dependants too); action `excluded`
    pub indexes_after_data: bool,      // true
    pub table_concurrency: u8,         // 1, clamped 1..=4; SQLite targets: 1
    opt pub batch_rows: Option<u32>,   // writer chunk rows, default 50_000, clamped 1..=65_536
    opt pub batch_mb: Option<u32>,     // writer piece MiB, default 8, clamped 1..=16 (effective size 4.1)
    opt pub target_lock_timeout_s: Option<u32>, // default 5, clamped 1..=600 (4.1)
    pub verify: VerifyMode,
    pub verify_skipped: bool,          // false; true: exact counts of skip_existing tables on both sides (5.6)
    pub on_unmappable: UnmappablePolicy,
    pub dry_run: bool,                 // start-time field
    opt pub confirm_production: Option<String>, // start-time field; must equal the target connection's name when tagged production
    pub source_session: SessionOpts,   // merged like export (request > connection > recommended); copy default: no statement timeout
    pub mysql: MysqlCopyOptions, pub pg: PgCopyOptions,
}
pub struct CopyRequest { pub source: CopyEndpoint, pub target: CopyEndpoint, pub selection: CopySelection,
                         #[serde(default)] pub options: CopyOptions, opt pub name: Option<String> }
pub struct CopyResumeRequest { pub run_id: String, #[serde(default)] pub acknowledged: Vec<String> /*ack keys*/,
    #[serde(default)] pub partial: ResumePartial, opt pub source_password: Option<String>,
    opt pub target_password: Option<String>, opt pub confirm_production: Option<String> }

// ---- plan
pub enum CopyObjectKind { Database, Schema, Extension, Collation, Type, Domain, Sequence, Function, Table, Partition, View,
    MaterializedView, Index, Constraint, ForeignKey, Trigger, Policy, Comment, Owner, Grant, TruncateGroup,
    SequenceValue, AutoIncrement, Refresh, Analyze }
pub enum PlanAction { Create, SkipExisting, Truncate, DropRecreate, Append, Run, Unsupported, Excluded, ResumeDone }
pub enum CopyPhase { Plan, PreData, Data, PostData, Finalize, Verify }
pub enum LoadMethod { PgCopyCsv, MysqlLoadData, MysqlInsert, SqliteInsert }
pub struct CopyIssue { pub code: String, pub severity: Severity, pub message: String, opt pub detail: Option<String>,
                       opt pub recommendation: Option<String>, opt pub item_id: Option<String> }
pub struct AckNeed { pub code: String, pub key: String, pub severity: Severity, #[serde(default)] pub items: Vec<String> }
pub struct CopyServerFacts { pub driver: String, pub version: String, pub database: String,
    pub endpoint: String /*non-secret: "host:port" (+ " via ssh host") or the SQLite path*/,
    opt pub charset: Option<String>, opt pub collation: Option<String>, opt pub user: Option<String>,
    #[serde(default)] pub notes: Vec<String> }
pub struct CopyEffective { opt pub charset: Option<String>, opt pub collation: Option<String>, opt pub engine: Option<String>,
    opt pub zero_dates: Option<ZeroDatePolicy> /*never Auto*/, opt pub time_zone: Option<String>,
    pub load_method: LoadMethod, pub table_concurrency: u8, opt pub chunk_bytes: Option<u64> /*MySQL (4.3)*/,
    pub consistent_snapshot: bool, pub source_sessions: u8, pub target_sessions: u8 }
pub struct PlanItem {
    pub id: String,                    // stable across re-plans; checkpoint key; format below
    pub kind: CopyObjectKind, opt pub parent: Option<String>,  // index/constraint/trigger/... -> its table item id
    opt pub schema: Option<String>, pub name: String,
    opt pub target_schema: Option<String>, pub target_name: String,
    pub action: PlanAction, pub phase: CopyPhase, pub order: u32,   // execution order inside the phase
    pub exists_in_target: bool,
    opt pub target_has_rows: Option<bool>,  // exact probe (3.7); absent when not probed
    opt pub target_est_rows: Option<u64>,
    pub has_data: bool, opt pub est_rows: Option<u64>, opt pub est_bytes: Option<u64>,
    opt pub source_ddl: Option<String>,     // FEDERATED CONNECTION values redacted
    #[serde(default)] pub target_sql: Vec<String>, // every statement BulkDB runs for this item; empty for skip/unsupported/excluded;
      // data items: the load template, data elided (`COPY … FROM STDIN (…)`, `LOAD DATA LOCAL INFILE '<stream>' …`, `INSERT INTO … VALUES <rows>`);
      // SequenceValue / AutoIncrement: a template with `<source value>` / `<target value>` placeholders, filled at finalize (5.1)
    #[serde(default)] pub warnings: Vec<CopyIssue>,
    #[serde(default)] pub depends_on: Vec<String>,
}
pub struct CopyTotals { pub tables: u32, pub objects: u32, pub create: u32, pub skip: u32, pub truncate: u32, pub drop_recreate: u32,
    pub append: u32, pub unsupported: u32, pub excluded: u32, opt pub est_rows: Option<u64>, opt pub est_bytes: Option<u64> }
pub struct SessionSql { #[serde(default)] pub source: Vec<String>, #[serde(default)] pub target: Vec<String> }
pub struct CopyPlan {
    pub plan_id: String,               // section 2 "Plan validity"
    pub family: CopyFamily, pub source: CopyServerFacts, pub target: CopyServerFacts, pub effective: CopyEffective,
    pub items: Vec<PlanItem>, pub warnings: Vec<CopyIssue>, pub blockers: Vec<CopyIssue>,
    #[serde(default)] pub acks: Vec<AckNeed>, pub session_sql: SessionSql, pub totals: CopyTotals,
    opt pub resume_of: Option<String>, pub elapsed_ms: u64,
}

// ---- progress / result
pub enum CopyItemState { Pending, Creating, Loading, Indexing, Verifying, Done, Skipped, Failed, Cancelled, Planned }
pub enum VerifyOutcome { NotRun, Match, Mismatch, Lossy /*counts equal, values changed (5.6)*/, Error }
pub enum ResumePartial { Fail /*def*/, Truncate, Skip }
pub struct ValueFix { pub column: String, pub code: String /*ZERO_DATE_NULL, ZERO_DATE_MIN, INVALID_DATE, ...*/, pub count: u64 }
pub struct CopyTableProgress { pub seq: u64, pub item_id: String, pub state: CopyItemState, pub rows_read: u64,
    pub rows_written: u64, pub bytes: u64, opt pub est_rows: Option<u64>, pub rate_rows_per_sec: f64,
    opt pub elapsed_ms: Option<u64>, opt pub method: Option<LoadMethod>, opt pub error: Option<ErrorPayload> }
pub struct CopyProgress { pub job_id: String, pub run_id: String, pub seq: u64 /*monotonic per job*/, pub phase: CopyPhase,
    pub items_total: u32, pub items_done: u32, pub items_failed: u32, pub items_skipped: u32, pub tables_total: u32,
    pub tables_done: u32, #[serde(default)] pub tables: Vec<CopyTableProgress>,
    pub full: bool }   // true: `tables` is the complete list; false (event delta): only entries changed since the last event
pub struct CopyTableResult { pub item_id: String, pub target_name: String, pub state: CopyItemState, pub rows_read: u64,
    pub rows_written: u64, opt pub target_count: Option<u64>, pub verify: VerifyOutcome,
    opt pub checksum_source: Option<String>, opt pub checksum_target: Option<String>, pub unmappable_chars: u64,
    pub data_warnings: u64, #[serde(default)] pub warning_samples: Vec<String> /*<= 10, redacted (5.2)*/,
    #[serde(default)] pub value_fixes: Vec<ValueFix>, #[serde(default)] pub problems: Vec<String> /*codes, 5.6*/,
    pub duration_ms: u64, opt pub method: Option<LoadMethod>, opt pub error: Option<ErrorPayload> }
pub struct CopyItemResult { pub item_id: String, pub state: CopyItemState, opt pub error: Option<ErrorPayload> }
pub enum LeftBehindKind { CreatedTable, PartialRows, EmptiedTable, MissingPostData, SequenceNotSet }
pub struct LeftBehind { pub item_id: String, pub kind: LeftBehindKind, opt pub rows: Option<u64> }
pub struct CopyResult { pub run_id: String, pub dry_run: bool, pub existing: ExistingTablePolicy /*as run*/,
    pub tables: Vec<CopyTableResult>, pub objects: Vec<CopyItemResult>, pub warnings: Vec<CopyIssue>,
    pub left_behind: Vec<LeftBehind>, pub verified: bool, pub problems: u32, pub skipped_existing: u32,
    pub resumable: bool, pub duration_ms: u64 }
pub struct CopyStatus { pub job_id: String, pub run_id: String, pub state: JobState,
    opt pub plan: Option<CopyPlan>,   // absent until the job has planned; progress.tables is empty and phase is `plan` until then
    pub progress: CopyProgress /*full*/, opt pub result: Option<CopyResult> }
pub struct CopyJobInfo { pub run_id: String, pub target_conn_id: String, pub target_conn_name: String,
    pub source_database: String, pub target_database: String, pub phase: CopyPhase, pub tables_total: u32,
    pub tables_done: u32, pub tables_failed: u32, pub dry_run: bool }
pub struct CopyRunInfo { pub run_id: String, pub label: String, pub source_conn_id: String, pub target_conn_id: String,
    pub source_database: String, pub target_database: String, pub started_at: String, opt pub finished_at: Option<String>,
    pub state: JobState, pub last_job_id: String, pub tables_total: u32, pub tables_done: u32, pub tables_failed: u32,
    pub resumable: bool, pub dry_run: bool, pub in_use: bool }
pub struct CopyRunDetail { pub info: CopyRunInfo, pub request: CopyRequest /*no passwords, no start-time fields*/,
    opt pub result: Option<CopyResult> }
```

**Plan item ids.** `<kind>:<path>`, path = the double-quoted (`"` doubled) source names joined by `.`: schema (PG only), then the table for per-table objects (index, constraint, foreign_key, trigger, policy; MySQL indexes too), then the name. Functions append `(<pg_get_function_identity_arguments>)`; comments, owners and grants use `<kind>:<target item id>`; a truncate group uses the path of its first table in id order; singletons use their kind (`database:"shop"`). Examples: `table:"app"."orders"`, `foreign_key:"app"."orders"."orders_customer_fk"`, `function:"app"."f"(integer, text)`, `truncate_group:"app"."customers"`.

**Shared type changes** (skeleton):
- `Connection.write_allowed: Option<bool>` (`opt`, TS `writeAllowed?: boolean`), stored only when `Some(true)`, resolved inside `ConfigStore::upsert` under the config lock against the re-read file: absent keeps the stored value; `false` clears; `true` sets only on an **existing** id (a new connection, empty id, never starts write-allowed; this covers duplicates and every other create path; DBeaver imports never set it). A change of driver, host, port, path, database, connection string or SSH endpoint (host, port, user) clears it unless the same save sends `true`. Only `markWriteAllowed`, 쓰기 허용 해제 and `conn edit --write-allowed` send the field: `lib/connection-actions.ts` `duplicateConnection` deletes `writeAllowed` from its copy and `ConnectionDialogForm.toInput` never includes it. ConnectionDialog shows "쓰기 허용 (DB 복사 대상) — 서버를 바꾸면 해제됩니다" with [해제] for write-allowed connections. Tests in `config.rs`/`service.rs` (new id ignores `true`; endpoint edit clears; edit plus explicit `true` keeps; a stale process cannot revive or drop the flag) and a vitest for the duplicate path.
- `JobKind::Copy`; `JobSummary.copy: opt Option<CopyJobInfo>`; for copy jobs `JobSummary.conn_id` / `conn_name` are the **source** and `label` = `name` or "{srcName}.{srcDb} → {tgtName}.{tgtDb}".
- `CoreEvent::{ConnectionsChanged, SettingsChanged(Settings), PromptResolved(PromptResolved { prompt_id })}`.
- `HostKeyPrompt` / `SshPasswordPrompt`: `opt conn_name`, `opt job_label`.
- `WindowInfo { label, title, opt conn_id, opt conn_name, focused: bool, index: u32 }`, `WindowInit { label, index: u32, opt open_conn_id }`, `InstanceInfo { pid: u32, instance_id: String, kind: String, started_at: String, version: String }`, `InstancesInfo { self_pid: u32, others: Vec<InstanceInfo> }`.
- `source/mod.rs`: `Query.max_batch_bytes: Option<usize>` (None = `BATCH_BYTES`; every reader honours it) and `SourceStream.replaced: Option<Arc<AtomicU64>>` (U+FFFD produced by decoding; filled by the MySQL readers with a non-UTF-8 `TextPlan` and by the PG transcoder of 3.5).

`JobEntry` gains `pub copy: Option<Arc<crate::copy::CopyJob>>` (`None` for other kinds); `summary()` fills `JobSummary.copy` from `copy.info()`. `JobRegistry::create_copy(conn_id, conn_name, label, make: impl FnOnce(&str /*job id*/) -> Arc<CopyJob>)` creates the job, so the first job's id can become the `run_id`. The job's `progress: Progress` counters carry the totals (rows, bytes), so `job:progress`, the JobChip and the ETA work unchanged; `expected_rows` = sum of `est_rows` of the tables to load.

**Internal contract types** (not IPC; `copy/types.rs`; copy-schema and copy-data meet here):

```rust
pub struct PlannedCopy { pub plan: CopyPlan, pub tables: BTreeMap<String /*item id*/, TableDataSpec> }
pub struct TableDataSpec { pub item_id: String,
    pub source_sql: String,            // one SELECT, BulkDB-generated (3.2 / 4.2)
    pub verify_sql: String,            // target-side read-back for checksum: target names, same column order and casts (5.6)
    pub raw_csv: bool,                 // PG->PG
    pub transcode: Option<PgSourceEncoding>, // PG SQL_ASCII source (3.5)
    pub target: TargetTable, pub columns: Vec<DataColumn>, pub est_rows: Option<u64>,
    pub first_timestamp_auto: bool,    // 4.0 source: first TIMESTAMP column is auto-set (3.3)
    pub fk_checks: bool }              // MySQL: load with foreign_key_checks = 1 (3.7)
pub struct TargetTable { pub schema: Option<String> /*MySQL: database; PG: schema; SQLite: None*/, pub name: String } // unquoted
pub struct DataColumn { pub source: String, pub target: String, pub value: ValueKind, pub read: SourceRead,
    pub zero_date: ZeroDateAction, pub invalid_date: ZeroDateAction, pub binary_attr: bool, pub max_bytes: Option<u64> }
pub enum SourceRead { Plain, CastChar, Concat, UnixTimestamp, DateFormat, YearNumber, FloatWiden }  // 3.2
pub enum ValueKind { Text, Binary, Bit, Integer, Decimal, Float, Bool, Date, DateTime, Timestamp, Time, Year, Json, Raw }
pub enum ZeroDateAction { NotTemporal, Null, Keep, Min }  // Min: DATE/DATETIME '1000-01-01[ 00:00:00]', TIMESTAMP epoch 1, YEAR 1901
pub struct ServerIdentity { pub key: String, pub system_identifier: Option<String>, pub server_uuid: Option<String>,
    pub replica_of: Option<String>, pub group_name: Option<String>, pub in_recovery: bool, pub read_only: bool,
    pub weak: bool /*MySQL < 5.6 hostname:port match only*/ }
pub struct TargetFacts { pub family: CopyFamily, pub flavor: String /*mysql|mariadb|postgres|sqlite*/, pub version: String,
    pub major: u16, pub minor: u16, pub patch: u16, pub database: String, pub user: Option<String>,
    pub vars: BTreeMap<String, String>, pub identity: ServerIdentity, pub endpoint: String }
pub struct MetaTable { pub columns: Vec<String>, pub rows: Vec<Vec<Option<String>>> }  // + fn get(&self, row, name) -> Option<&str>
pub enum CheckpointState { Pending, Creating, Created, Loading, Loaded, Done, Failed, Skipped }
pub struct ResumeState { pub run_id: String, pub items: BTreeMap<String, (CheckpointState, bool /*created_by_run*/, u64 /*rows_written*/)>,
                         pub partial: ResumePartial }
pub struct Budgeted<T> { pub item: T, pub rows: u64, pub permit: tokio::sync::OwnedSemaphorePermit }
pub enum LoadInput { RawCsv(mpsc::Receiver<Budgeted<Bytes>>), Arrow(mpsc::Receiver<Budgeted<RecordBatch>>) }  // depth 1
pub struct LoadOutcome { pub rows: u64, pub warnings: u64, pub skipped: u64, pub samples: Vec<String>, pub unmappable: u64,
    pub value_fixes: Vec<ValueFix>, pub method: LoadMethod }
```

**Traits and seams.** `copy::target::TargetSession` (copy-data):

```rust
#[async_trait] pub trait TargetSession: Send + Sync {
    fn facts(&self) -> &TargetFacts;
    async fn query_meta(&self, sql: &str) -> Result<MetaTable>;        // reads only (5.2)
    async fn execute(&self, sql: &str) -> Result<u64>;                  // one generated statement; affected rows
    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome>;
    async fn count_rows(&self, t: &TargetTable) -> Result<u64>;
    async fn cancel_running(&self);                                    // PG cancel request / MySQL KILL QUERY from a side connection / SQLite interrupt
    fn configure(&self, cfg: &WriterConfig) {}                         // job writer settings (chunks, lock timeout, MySQL charset per table)
    fn take_failed_outcome(&self) -> Option<LoadOutcome> { None }      // after a failed / cancelled load: what it committed (5.4)
}
#[async_trait] pub trait TargetOpener: Send + Sync {
    async fn open(&self, conn: &Connection, ep: &CopyEndpoint, app_name: &str) -> Result<Box<dyn TargetSession>>;
}
```

- `BulkDbOptions.copy_target_opener: Option<Arc<dyn TargetOpener>>` (hidden from docs; `None` = the real opener). This is how `copy_tests.rs`, an integration test that can only call the public `BulkDb::copy_*` methods, substitutes a target.
- Implementations: `PgTarget`, `MysqlTarget`, `SqliteTarget`, and the public test double `copy::testkit::RecordingTarget`. Its **frozen** API (copy-data may only extend it): `RecordingTarget::new(facts)`, `.answer(sql_substring, MetaTable)`, `.statements() -> Vec<(session_no, String)>`, `.payloads() -> Vec<(item_id, Bytes)>`; `CannedMeta::new()`, `.answer(sql_substring, MetaTable)`. RecordingTarget encodes Arrow input through the same pure `copy::data::mysql_encode` as `MysqlTarget` (normalisation, LOAD DATA fields or INSERT literals, chosen by `facts.vars["local_infile"]`), so recorded payloads are what a server would receive; it answers unmatched `query_meta` with an empty `MetaTable`, `count_rows` returns the rows it recorded, and checksum verification is not supported on it (`verify: checksum` → `INVALID`).
- `copy::schema::SourceMeta` (copy-schema): `#[async_trait] trait SourceMeta { async fn query(&self, sql: &str) -> Result<MetaTable>; }`, picked by `schema::source_meta(live: &LiveConnection) -> Result<Box<dyn SourceMeta + '_>>`: PG `PgDatabase.client` with `simple_query` (text values); MySQL modern or legacy `source::mysql_raw(db)?.meta_query`, decoded with `facts().decode_meta` (raw bytes for the sampler of 3.2); SQLite its own read-only rusqlite connection on `SqliteDatabase::path()`; `copy::testkit::CannedMeta` for tests. All of them run on read-only sessions (4.1) and refuse text with more than one statement.
- `plan::build_plan(input: PlanInput<'_>) -> Result<PlannedCopy>` with `PlanInput { req: &CopyRequest, source: &dyn SourceMeta, source_conn: &Connection, source_server: &ServerInfo, mysql_source: Option<&ServerFacts>, target: &dyn TargetSession, target_conn: &Connection, resume: Option<&ResumeState>, cancel: &CancellationToken }`. It creates **every** item (including the `Database` item for a missing MySQL database and `TruncateGroup` items), the translation issues and the per-family catalog checks of section 3 (including PG's `TGT_PRIVILEGE`). copy-data's `copy::preflight::finalize(&mut PlannedCopy, ...)` only adds the section 5.3 issues (including MySQL's `TGT_PRIVILEGE`), fills `acks` and sets `plan_id`.
- Finalize values (copy-schema, frozen): `schema::read_finalize_values(src: &dyn SourceMeta, items: &[&PlanItem]) -> Result<BTreeMap<String, (i128, bool /*is_called*/)>>`, `schema::read_target_floor(tgt: &dyn TargetSession, item: &PlanItem) -> Result<Option<i128>>` and `translate::finalize_sql(item: &PlanItem, source: (i128, bool), floor: Option<i128>) -> String`. copy-data's exec calls them at finalize on the job's plan meta session, which is kept open and never streams.

## 3. Translation rules (owner copy-schema)

### 3.1 Supported versions

| family | sources | targets | notes |
|---|---|---|---|
| MySQL | 4.0 (legacy client; 3.23 by the same rules, untested), 4.1, 5.0-5.7, 8.0, 8.4, 9.x | 5.0-5.7, 8.0, 8.4, 9.x | A 4.x target is rejected (`TARGET_VERSION_UNSUPPORTED`: the legacy client never writes). 6.0 alpha: 5.x rules + `UNTESTED_VERSION` (warning). MariaDB: 5.7 rules + `MARIADB` (warning). Source newer than target: `DOWNGRADE` (danger). 4.1.0-4.1.2 sources follow the 4.0 rules for TIMESTAMP (no per-session time_zone before 4.1.3). |
| PostgreSQL | 10-18 | 10-18 | 15→16, 15→17 and same-version copies are tested. Older target: `DOWNGRADE` (danger). Below 10: `SOURCE_VERSION_UNSUPPORTED` / `TARGET_VERSION_UNSUPPORTED`. |
| SQLite | any | any | Renames are `INVALID`. |

### 3.2 MySQL source model

The same SHOW statements work on every version, so one parser serves 4.0 through 9.x. For 4.0 sources these exact strings are the contract with the fake-server test (section 9):

| purpose | SQL |
|---|---|
| tables, engine, estimates | ``SHOW TABLE STATUS FROM `<db>` `` (4.0 column `Type`, 4.1.2+ `Engine`: look columns up by name) |
| table definition | ``SHOW CREATE TABLE `<db>`.`<t>` `` |
| server charset / zone (4.0) | `SHOW VARIABLES LIKE 'character_set'`, `SHOW VARIABLES LIKE 'timezone'` (facts line only) |
| views (5.0+) | ``SHOW FULL TABLES FROM `<db>` WHERE Table_type = 'VIEW'``, ``SHOW CREATE VIEW`` |
| triggers (5.0.10+) | ``SHOW TRIGGERS FROM `<db>` ``; ``SHOW CREATE TRIGGER`` from 5.1.21. On 5.0.10-5.1.20 the statement is built from the SHOW TRIGGERS columns (`CREATE TRIGGER <Trigger> <Timing> <Event> ON <Table> FOR EACH ROW <Statement>`, its `sql_mode` column when present); when a column is missing the trigger is `unsupported` with `TRIGGER_UNREADABLE` (warning). |
| data | ``SELECT <col list> FROM `<db>`.`<t>` `` with the per-column reads below; generated columns are left out |
| charset sample (planning only) | ``SELECT CAST(`c` AS BINARY) FROM `<db>`.`<t>` WHERE `c` <> '' LIMIT 1000`` (4.0: ``SELECT `c` FROM … LIMIT 1000``) |
| next AUTO_INCREMENT (finalize) | ``SHOW TABLE STATUS FROM `<db>` LIKE '<t>'`` with `\`, `_` and `%` escaped and `Name` matched exactly; 8.0+ sources run `SET SESSION information_schema_stats_expiry = 0` first (else the value is up to 24 h old) |

**Per-column reads** (`DataColumn.read`):

| column | read as | why |
|---|---|---|
| DATE, DATETIME, TIME | ``CAST(`c` AS CHAR) AS `c` `` (4.0.2+), ``CONCAT(`c`)`` before 4.0.2 | keeps zero and invalid dates, which the Arrow value path would turn into NULL |
| TIMESTAMP, source < 4.1.3 | ``UNIX_TIMESTAMP(`c`) AS `c` `` | the stored instant; 0 only for the zero value. On 4.0, `TIMESTAMP(N)` with N < 14 also prints only N digits as text (`timestamp(8)` → 'YYYYMMDD', `(6)` → 'YYMMDD'), which would drop the time of day; and there is no session time_zone before 4.1.3, so no offset is needed (Korea's 1987-88 DST included). |
| TIMESTAMP, source ≥ 4.1.3 | CAST as above, session `time_zone = '+00:00'` | UTC text |
| YEAR | ``(`c` + 0) AS `c` `` | a 4-digit number, 0 for the zero year (YEAR(2) prints both 2000 and 0000 as '00') |
| FLOAT without (M,D), text-protocol source (4.0 legacy client) | ``(`c` + 0e0) AS `c` `` | widened to DOUBLE: ≥ 14 significant digits instead of about 6 (a float needs 9) |
| DOUBLE, text-protocol source | plain | 4.0 prints at most 15 significant digits and no 4.0 expression prints 17: `FLOAT_TEXT_PRECISION` (warning) per column ("values beyond 15 significant digits are rounded") |
| everything else | plain | 4.1+ sources use the binary protocol and are exact |

The TIMESTAMP and FLOAT expressions are pinned by the optional real-4.0 CI test (section 9). If UNIX_TIMESTAMP proves wrong there, the fallback is `DATE_FORMAT(c,'%Y-%m-%d %H:%i:%s')` read as local wall time and converted with the fixed offset `946684800 − UNIX_TIMESTAMP('2000-01-01 00:00:00')` or `mysql.time_zone`; only that fallback needs an offset, and only it raises `TZ_DST` (warning) when the July offset differs.

**Parsing.** `translate::mysql::parse_create_table(text) -> MyTable` covers: columns (name, type + arguments, UNSIGNED/ZEROFILL, CHARACTER SET/COLLATE, 4.0 `binary` attribute, NULL, DEFAULT including expression defaults `DEFAULT (expr)` (8.0.13+), `auto_increment`, `on update CURRENT_TIMESTAMP` on TIMESTAMP and DATETIME, COMMENT, GENERATED, INVISIBLE, SRID); keys (PRIMARY, UNIQUE, KEY, FULLTEXT, SPATIAL, prefix lengths, `DESC` parts, functional key parts `((expr))` (8.0.13+), `INVISIBLE`, `KEY_BLOCK_SIZE`, `WITH PARSER`, `COMMENT`); CHECK constraints (8.0.16+, MariaDB 10.2+); FKs; table options (`TYPE=`/`ENGINE=`, DEFAULT CHARSET, COLLATE, AUTO_INCREMENT, COMMENT, ROW_FORMAT, KEY_BLOCK_SIZE, COMPRESSION, STATS_PERSISTENT / STATS_AUTO_RECALC / STATS_SAMPLE_PAGES, MAX_ROWS, AVG_ROW_LENGTH, PACK_KEYS, DELAY_KEY_WRITE, CHECKSUM, RAID_*, UNION, CONNECTION); partitioning, also inside `/*!50100 … */`. Each clause is translated or dropped with an issue (`DOWNGRADE` per clause when the target lacks it, e.g. CHECK below 8.0.16 is ignored by the server, functional keys below 8.0.13 are refused). Any clause the parser does not recognise is `UNPARSED_CLAUSE` (item blocker) instead of being dropped.

**Text decoding.** All text (ENUM/SET values, DEFAULT strings, COMMENTs, data) arrives as UTF-8 through the connection's `TextPlan` (docs/mysql-compat.md §5): a 4.0 `euc_kr` server, or `encoding=euc-kr` with CP949 in latin1 columns, yields correct Korean. The **source text charset** of a column (EUC-KR, latin1 or UTF-8 as the TextPlan decodes it) also sizes the target (3.3). A 4.0 server charset other than `euc_kr`/`latin1`/`utf8` with `encoding=auto` (which decodes it as latin1) is `CHARSET_UNSUPPORTED` (blocker) unless the connection's `encoding` field is set explicitly. On 4.1+ with `encoding=auto` the server converts every column to UTF-8.

**Charset sample.** A decode that produces valid-looking text cannot be detected later (no U+FFFD, counts and checksums match), so the planner samples. It runs when any text column's effective decode is latin1 (a 4.0 latin1 server under `encoding=auto`, latin1 columns on 4.1+ under `auto` or `latin1`), for `euckr` columns, for 4.0 binary-attribute columns and for BLOB/TINYBLOB columns. Per column it reads at most 1,000 non-empty values as raw bytes (the sample SQL above), from the 20 largest tables, at most 200 columns and 4 MiB in total, on the meta session. The values stay inside the planner: never in IPC, logs, history or issue text. Tests and issues:
- latin1 column whose high-bit values decode as CP949 without error (≥ 90 % of them, with Hangul syllables in the result) → `SRC_LATIN1_KOREAN`: a **blocker** when the connection's encoding is `auto` (recommendation: set the source connection's encoding to euc-kr), a danger warning that needs an acknowledgement when the user set `latin1` explicitly.
- euckr column whose high-bit values are well-formed UTF-8 forming Hangul → `SRC_EUCKR_HOLDS_UTF8` with the same severity rule (recommendation: encoding utf8 on 4.0, or exclude the column's table).
- 4.0 `varchar(n) binary` / `char(n) binary` column whose values do not decode cleanly (raw bytes such as AES_ENCRYPT or MD5 output) → `BINARY_ATTR_UNDECODABLE` (item blocker; recommendation: `mysql.columns` `treat_as: binary`, which makes it `varbinary(n)` / `binary(n)` loaded as bytes). At run time a U+FFFD in a binary-attribute column fails the table with the same code.
- BLOB/TINYBLOB column whose values decode as CP949 Hangul → `BLOB_HOLDS_TEXT` (info; recommendation `treat_as: text`, which makes it the TEXT class of the same byte size, decoded like the table's text).

`mysql.columns[].treat_as` (CLI `--column-as t.c=binary|text`) overrides the column's kind for DDL and data.

### 3.3 MySQL types, charsets, engines (source → target)

**Effective charset.** Default utf8mb4 when the target is ≥ 5.5.3, else utf8. A user charset must exist in the target's `SHOW CHARACTER SET` (`CHARSET_UNSUPPORTED`, blocker). On a utf8 (3-byte) target, 4-byte characters (only possible from a UTF-8 source) are replaced by `?` by the writer and counted (`UTF8MB3_ONLY`, warning; 5.6 outcome). This holds on every target version whenever the table's effective charset is utf8 / utf8mb3: a server before 5.5.3 (utf8 connection), a user `charset` of utf8 / utf8mb3 (plan warning), or a `table_overrides` charset of utf8 / utf8mb3 (item warning on that table). The writer gets the charset from `WriterConfig.charset` (`CopyEffective.charset`) and `WriterConfig.table_charsets` (overrides by item id), never from server variables; `RecordingTarget` and the MySQL checksum encoder use the same rule. EUC-KR, CP949 and latin1 hold only BMP characters. `mysql.table_overrides` set charset and collation per table (UI: the action on an issue row; CLI `--table-charset t=cs`, `--table-collation t=coll`).

**Default collation** of utf8mb4: 8.0/8.4/9.x `utf8mb4_0900_ai_ci`; 5.5.3-5.7 and MariaDB `utf8mb4_general_ci`; < 5.5.3 `utf8_general_ci`. The preset `collation: "legacy"` (UI "기존 정렬과 호환 (PAD SPACE)", CLI `--collation legacy`) resolves to `utf8mb4_unicode_520_ci` on 5.6+ targets, else `utf8mb4_general_ci`. Every per-column CHARACTER SET is dropped and text columns take the table charset; an explicit `CHARACTER SET binary` (or a binary type) stays binary. A user charset (`mysql.charset`) or collation (`mysql.collation`, or `table_overrides[].collation`) must match `^[A-Za-z0-9_]+$` and exist in the target's `SHOW CHARACTER SET` / `SHOW COLLATION`; an unknown name gives `CHARSET_UNSUPPORTED` (blocker) or `COLLATION_UNSUPPORTED` (blocker). The `legacy` collation preset is not validated against `SHOW COLLATION`.

| source collation / attribute | target collation |
|---|---|
| `euckr_korean_ci`, 4.0 `euc_kr` (no collation), `latin1_swedish_ci`, other `*_general_ci` / default | the effective default, or the user's `collation` |
| `*_bin`, 4.0 `binary` attribute (`varchar(20) binary`) | `<charset>_bin` |
| `utf8_general_ci`, `utf8_unicode_ci`, `utf8mb3_*` | same family widened: `utf8mb4_general_ci`, `utf8mb4_unicode_ci` |
| `latin1_general_cs`, other `_cs` | 8.0+: `utf8mb4_0900_as_cs`; older: `utf8mb4_bin` |
| `utf8mb4_0900_*` on a < 8.0 target | `utf8mb4_unicode_520_ci` (5.6+) else `utf8mb4_unicode_ci`, + `DOWNGRADE` |

A text column whose collation semantics change (any EUC-KR/latin1 → UCA) adds `COLLATION_CHANGED` (info). When the source collation is PAD SPACE (`euckr_korean_ci`, `latin1_swedish_ci`, `utf8mb4_general_ci`, every pre-8.0 collation) and the target is NO PAD (`utf8mb4_0900_*`), the issue says so: `WHERE c = 'abc '` and uniqueness with trailing spaces behave differently (recommendation: the `legacy` preset). On UNIQUE/PRIMARY keys over text it adds `COLLATION_UNIQUE_RISK` (warning: values that differ only by width, case, accent or trailing spaces, e.g. 'Ａ' vs 'A', collide under `_ai_ci`; recommendation: `utf8mb4_bin` for that table through `table_overrides`). Such duplicates fail the load (5.6).

| source type | target | rule / issue |
|---|---|---|
| `char(n)` / `varchar(n)` (4.0: n **bytes**; 4.1+: n chars) | same n, in characters | never shrinks; every 4.0 n-byte value fits n chars. A `varchar(n)` whose n × bpc exceeds 65,535 bytes cannot exist and becomes `MEDIUMTEXT` (TEXT holds only 65,535 bytes; `VARCHAR_TO_TEXT`, warning) |
| `tinytext` / `text` / `mediumtext` (limits in **bytes**) | one class up when the target's bytes per char exceed the source's (EUC-KR 2 → utf8mb4 4; latin1-held CP949 likewise) | `TEXT_PROMOTED` (info); `longtext` stays |
| `timestamp(N)` (4.0/4.1 display width 2-14) | `timestamp` | 5.5 rejects the width; 5.6.4+ would read N as fsp. Read as epoch (3.2); semantics below |
| `year(2)` | `year` | removed in 5.7.5; read as `c + 0` (3.2), so the server's widening is used and the zero year stays 0 and follows the zero-date policy (`YEAR2_WIDENED`, info) |
| `decimal(M,D)` from a source < 5.0.3 | `decimal(min(M+1,65), min(D,30))` | old DECIMAL let a positive value use the sign byte for one more digit (`DECIMAL_WIDENED`, info); M > 65 → `DECIMAL_PRECISION` (warning) |
| `int(11)` and other display widths | 8.0.17+: width dropped except `tinyint(1)` and ZEROFILL columns | deprecated in 8.0.17, no issue |
| `ZEROFILL`, `UNSIGNED` on FLOAT/DOUBLE/DECIMAL, `float(M,D)` | kept | `DEPRECATED_SYNTAX` (info) on 8.0.17+ |
| `bool` / `boolean` | `tinyint(1)` | as SHOW CREATE prints it |
| `enum(...)` / `set(...)` | same values as UTF-8 literals via `mysql_common::quote_str` | Korean values work; a duplicate under the new ci collation is `ENUM_DUPLICATE` (item blocker; recommendation `_bin` collation for that table). A source that ran without `STRICT_*` could have stored `''` (the out-of-range sentinel) in an ENUM column; the strict target refuses it. By default the table fails with `ENUM_ERROR_VALUE`; `mysql.enum_error: null` loads `''` as NULL instead (counted per column as `ENUM_ERROR_NULL` in `value_fixes`; a NOT NULL column still fails). Preflight probes (`ENUM_ERROR_VALUE`, warning when `enum_error: fail`; info when `enum_error: null`, env) identify tables with such rows. |
| `json`, generated, invisible columns, `srid` (8.0 sources) | same on 8.0+; `json` → `longtext` below 5.7.8; generated/invisible on older targets → `UNSUPPORTED_COLUMN` (item blocker) | generated columns are never loaded |
| `geometry` and other spatial types | same | loaded as internal-format bytes through `UNHEX` (4.3); SPATIAL keys on InnoDB need 5.7+ (engine rule) |
| `bit(n)`, binary, varbinary, blobs | same | loaded through user variables (4.3); MEDIUM/LONG blobs and texts with `max_allowed_packet` < 64 MiB: `BLOB_PACKET_LIMIT` (warning, the limit in detail) |

**TIMESTAMP semantics** (`TIMESTAMP_SEMANTICS`, info).
- Every TIMESTAMP value is written as UTC text and every target session and every DDL statement runs under `time_zone = '+00:00'` (4.3), so the minimum '1970-01-01 00:00:01' is in range. `copy::data::values::timestamp_min_literal(offset)` computes the minimum for a session offset ('+00:00' → '1970-01-01 00:00:01', '+09:00' → '1970-01-01 09:00:01', '−05:00' → '1969-12-31 19:00:01'); it is unit-tested for several offsets and BulkDB always calls it with '+00:00'.
- 4.0 source: the first TIMESTAMP column was auto-set on insert and update; it becomes `timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`, and its zero values load as the minimum under every zero-date policy except `keep` (`ZERO_DATE_MIN` counted). Other 4.0 TIMESTAMPs were NOT NULL defaulting to zero: the zero-date rule (3.4) applies.
- TIMESTAMP DEFAULT literals from sources < 4.1.3 are local wall time: the planner converts them to UTC with `SELECT UNIX_TIMESTAMP('<literal>')` on the source before writing the DDL.
- 4.1+ sources keep what SHOW CREATE prints. Every TIMESTAMP is written with explicit NULL/NOT NULL and DEFAULT, so the target's `explicit_defaults_for_timestamp` (ON in 8.0) changes nothing.

**Defaults and AUTO_INCREMENT.** String defaults are re-quoted. `DEFAULT CURRENT_TIMESTAMP` and `ON UPDATE` on DATETIME need 5.6.5+; older targets drop them (`DEFAULT_DROPPED`, warning). Expression defaults need 8.0.13+ (`DOWNGRADE` per clause). `auto_increment` is kept and its key stays inline in CREATE TABLE (MySQL requires the AUTO column to be indexed). Finalize item `AutoIncrement`: `ALTER TABLE … AUTO_INCREMENT = <source value>` only when the source's next value (3.2 re-read) exceeds the target's. InnoDB targets before 8.0 reset the counter to max+1 on restart, so preserved gaps are lost after a restart (`AUTOINC_NOT_PERSISTED`, info).

**MyISAM grouped AUTO_INCREMENT.** AUTO_INCREMENT on a non-leading column of a composite key (per-group counters) has no InnoDB equivalent: the table keeps MyISAM whatever the engine choice (`AUTOINC_GROUPED`, warning).

**Engines.** `auto` = InnoDB on targets ≥ 5.5, else the source engine; `keep` = source engine; `innodb` / `myisam` force one. The session `sql_mode` has `NO_ENGINE_SUBSTITUTION`, so an unavailable or disabled engine fails the CREATE instead of becoming the default engine.
- Renames: `HEAP` → `MEMORY`; `ISAM`/`BDB`/`BERKELEYDB` → the chosen engine (`ENGINE_CHANGED`, info); `MRG_MYISAM`/`MERGE` and `FEDERATED` → `unsupported` (`UNSUPPORTED_OBJECT`, warning; a FEDERATED `CONNECTION='mysql://user:pass@…'` is redacted in `source_ddl`, IPC and logs).
- With InnoDB chosen, a table with FULLTEXT on a < 5.6 target or SPATIAL on a < 5.7 target keeps MyISAM (`ENGINE_KEPT_MYISAM`, warning).
- Dropped options: `TYPE`, `RAID_*`, `UNION`, `MAX_ROWS`, `AVG_ROW_LENGTH`; `PACK_KEYS`, `DELAY_KEY_WRITE`, `CHECKSUM` when the engine changes (`TABLE_OPTION_DROPPED`, info).
- `ROW_FORMAT=DYNAMIC` on InnoDB on 5.7.9+, or on 5.5/5.6 when `innodb_file_format=Barracuda` **and** `innodb_file_per_table=ON` (otherwise the server silently uses COMPACT); otherwise left out.
- Partitioning (5.1+ sources, also inside `/*!50100 … */`) is kept verbatim when the target supports it for the chosen engine (8.0+: InnoDB and NDB only); otherwise `PARTITION_UNSUPPORTED` (item blocker; recommendation engine innodb or exclude).
- The table ends `ENGINE=<e> DEFAULT CHARSET=<cs> COLLATE=<coll>` [`ROW_FORMAT=…`] [`COMMENT=…`] [partitioning].
- After every CREATE TABLE the writer reads ``SHOW CREATE TABLE`` and compares ENGINE and ROW_FORMAT; a difference fails the item (`DDL_SUBSTITUTED`).

**DDL placement** (InnoDB unless noted).
- CREATE TABLE holds the columns, the PRIMARY KEY and the AUTO_INCREMENT key (adding a PK after the data rebuilds the clustered index), table options and partitioning.
- All other keys (UNIQUE, KEY, FULLTEXT, SPATIAL) of a table go into **one** `ALTER TABLE … ADD …, ADD …` in post_data after its data. With `indexes_after_data = false` they go into CREATE TABLE.
- MyISAM targets: every key inside CREATE TABLE, `ALTER TABLE … DISABLE KEYS` before the load and `ENABLE KEYS` after (one sort build of the non-unique keys).
- FKs: one `ALTER TABLE … ADD CONSTRAINT …, ADD CONSTRAINT …` per child table after **all** tables are loaded, under `foreign_key_checks = 0` (INPLACE on 5.6+, no rebuild, no long metadata lock), then validated by orphan probes in verification (5.6). Targets < 5.6 rebuild the table on every ALTER, so there the keys and FKs of a table go into a single ALTER after all loads (`FK_VALIDATE_REBUILD`, info, with the table sizes, so the cost is visible).
- An FK whose child or parent target engine is not InnoDB is `unsupported` with `FK_ON_MYISAM` (warning: MySQL parses and ignores it on MyISAM, and an InnoDB child cannot reference a MyISAM parent).
- Triggers come last in post_data, so they never fire during the load. Views are created after all tables in dependency order (below).
- Post-data work runs in the same worker pool as the loads (4.1).

**Index and row limits.** Bytes per char (bpc): utf8mb4 4, utf8 3.
- InnoDB key part: 3072 bytes with DYNAMIC/COMPRESSED (5.7.7+ default, or 5.5/5.6 with `innodb_large_prefix=ON` + Barracuda + file_per_table), else 767 bytes per part; 3072 bytes per key. MyISAM: 1000 bytes per key.
- A non-unique key part over the limit gets a prefix `col(floor(limit/bpc))` (191 under 767, 768 under 3072): `INDEX_PREFIX` (warning).
- For UNIQUE/PRIMARY keys a prefix changes uniqueness: `UNIQUE_PREFIX` (danger; recommendation: a target with DYNAMIC rows (5.7.7+), a shorter column, or charset utf8 for that table through `table_overrides`).
- **Row limits**, from target byte maxima (n × bpc): (a) MySQL's 65,535-byte limit over the non-TEXT/BLOB columns; (b) InnoDB's in-row limit, about half a page (`innodb_page_size` / 2 − 66, 8,126 bytes at 16 KiB), computed the way `innodb_strict_mode` checks it: fixed-size columns in full; variable columns with a maximum ≤ 255 bytes in full; longer ones and TEXT/BLOB 768 + 20 bytes on COMPACT/REDUNDANT (the 5.0-5.6 default) and 40 bytes on DYNAMIC/COMPRESSED. With `fix_row_size` on: (a) the widest VARCHARs in no key become `TEXT` when n × bpc ≤ 65,535, else `MEDIUMTEXT` (`ROW_SIZE_FIXED`, warning); (b) the table gets `ROW_FORMAT=DYNAMIC` where the target supports it (`ROW_FORMAT_DYNAMIC`, info). What remains, or anything with `fix_row_size` off, is `ROW_SIZE_LIMIT` / `INNODB_ROW_SIZE` (item blocker; recommendation: a 5.7+ target, engine myisam, or exclude). This matters most with `innodb_strict_mode=OFF`, where the CREATE succeeds and the load fails partway.

**Target policies that refuse legacy shapes.**
- 8.4+/9.x `restrict_fk_on_non_standard_key=ON` (default) refuses FKs whose parent columns are not a full PRIMARY/UNIQUE key: such FK items are `unsupported` with `FK_NON_STANDARD_KEY` (warning).
- `sql_require_primary_key=ON` (8.0.13+): a table without a PRIMARY KEY is `TGT_REQUIRES_PK` (item blocker). BulkDB never changes this variable.
- `log_bin=ON` and `log_bin_trust_function_creators=OFF` with triggers selected: `TGT_BINLOG_TRIGGERS` (warning: CREATE TRIGGER may need SUPER). `log_bin=ON` alone: `TGT_REPLICATION_LAG_RISK` (info: every chunk is one binlog transaction, 4.3).

**Identifiers.** Always quoted with `` ` `` (doubled inside) by `catalog::mysql::quote_ident`, so words that became reserved never break BulkDB's SQL. `RESERVED_WORD` (info) names tables/columns reserved on the target, for the applications' sake. Examples: 5.x `CONDITION`, `TRIGGER`, `RANGE`, `PARTITION`, `SIGNAL`, `GET`, `GENERATED`, `VIRTUAL`, `STORED`; 8.0 `RANK`, `GROUPS`, `WINDOW`, `ROW`, `ROWS`, `ROW_NUMBER`, `LAG`, `LEAD`, `OVER`, `RECURSIVE`, `SYSTEM`, `CUBE`, `EMPTY`, `EXCEPT`, `FUNCTION`, `GROUPING`, `LATERAL`, `OF`; 8.0.17 `ARRAY`, `MEMBER`; 8.0.31 `INTERSECT`; 8.4 `MANUAL`, `PARALLEL`, `QUALIFY`, `TABLESAMPLE`. Full per-version lists (from the MySQL keyword pages) live in `translate/mysql_reserved.rs`.

**Names.** `name_case: lower` lower-cases table names; so does a target with `lower_case_table_names` ≠ 0. A collision (`Users` / `users` from a case-sensitive 4.0 on Linux) is `NAME_COLLISION` (blocker). Renamed tables referenced by copied views/triggers are not rewritten: `RENAMED_REFS` (warning).

**Views, triggers, FKs** (5.x+ sources). DDL from SHOW CREATE.
- **Database qualifiers.** MySQL stores view definitions fully qualified (``select `srcdb`.`t`.`a` … from `srcdb`.`t` ``). A token-aware lexer (skipping string literals and comments) rewrites every backquoted identifier equal to the source database name in qualifier position (followed by `.`) to the target database name. Trigger bodies are not rewritten (they may use unquoted or computed names): any reference to the source database name in a trigger body is `DB_QUALIFIED_REFS`, a blocker when a database with the source's name exists on the target server (on the same server that is the source itself, so copied triggers would write into it), a warning otherwise; recommendation: exclude the trigger or fix it by hand afterwards.
- **View order.** MySQL has no `pg_depend`: the planner orders views topologically by the `` `db`.`name` `` references in each definition (always qualified). As a safety net, a CREATE failing with `ER_NO_SUCH_TABLE` (1146) is retried in another pass until a pass makes no progress.
- **Triggers** are created under their original `sql_mode` (SHOW CREATE TRIGGER / `information_schema.TRIGGERS`, minus modes the target does not know), because a trigger keeps the creating session's sql_mode forever. Their `character_set_client` is the session's utf8mb4/utf8, since BulkDB sends the body as UTF-8 text, and `collation_connection` maps like a column collation. The writer restores its session settings afterwards.
- **DEFINER.** Removed by default, and `SQL SECURITY DEFINER` stays, so views and triggers then run with the copy account's privileges (usually far broader): `DEFINER_CHANGED` (warning) lists the affected objects. `mysql.keep_definer` keeps `DEFINER=` for accounts that exist on the target (``SELECT 1 FROM mysql.user WHERE User = '…' AND Host = '…'``; creating them needs SUPER or SET_USER_ID); the others keep the warning.
- Routines and events: one `unsupported` item each with `MYSQL_ROUTINES_NOT_COPIED` (warning), kind `function` for procedures, functions and events alike, id `function:"<name>"(procedure|function|event)` (the three namespaces are separate). Listed for selection `all` (5.0+; events 5.1.6+); a `tables` selection lists none.

### 3.4 Zero dates, invalid dates and time zone (MySQL)

`zero_dates: auto` resolves to `null` when the target's global `sql_mode` contains `NO_ZERO_DATE`, or `NO_ZERO_IN_DATE` with `STRICT_*` (5.7+/8.x defaults); otherwise `keep` (5.0-5.6 defaults).

| policy | DDL | data | issue |
|---|---|---|---|
| `null` | temporal NOT NULL columns become NULL; zero defaults become `DEFAULT NULL` | '0000-00-00…', partial zero dates ('2020-00-15') and the zero YEAR → NULL | `ZERO_DATE_NULL` (warning, per column) |
| `keep` | kept; the target session `sql_mode` drops `NO_ZERO_DATE` and `NO_ZERO_IN_DATE` and adds `ALLOW_INVALID_DATES`, staying strict (4.3) | kept | `ZERO_DATE_KEEP` (warning: applications on strict servers fail to write such rows) |
| `min` | NOT NULL kept; zero defaults become the minimum | zero → '1000-01-01[ 00:00:00]' (DATE/DATETIME), the TIMESTAMP minimum (3.3), 1901 (YEAR); partial dates replace each 00 part by 01 ('2020-05-00' → '2020-05-01', '2020-00-00' → '2020-01-01') | `ZERO_DATE_MIN` (warning) |

- **Keys.** A PRIMARY KEY cannot hold NULL (5.7+ refuses the DDL; older servers silently restore NOT NULL) and NULLs never collide in a UNIQUE key: temporal PRIMARY KEY columns use `min` even under `null`, and UNIQUE keys over such columns get `ZERO_DATE_IN_KEY` (warning; PK case: the action used, UNIQUE case: duplicates may load; recommendation `min`).
- **Invalid calendar dates.** MySQL before 5.0.2, including every 4.0, accepted impossible dates such as '2004-02-31' (it checked only month 0-12 and day 0-31). `normalize_temporal` validates DATE, DATETIME and TIMESTAMP values with chrono. Under `keep`, DATE/DATETIME keep them (`ALLOW_INVALID_DATES`); a TIMESTAMP cannot hold them and falls back to NULL when nullable, else the minimum. Under `null` and `min` they follow the column's action. Every replacement is counted per column (`INVALID_DATE`, a 5.6 problem, since the plan cannot know them in advance).
- **Counting.** `normalize_temporal` applies the action once and counts each replacement into `LoadOutcome.value_fixes` (column, code, count), which the result shows per table.

**Time zone.** Sources ≥ 4.1.3 read under `'+00:00'`; sources < 4.1.3 return TIMESTAMPs as epoch values (3.2); `normalize_temporal` formats epochs as UTC text with chrono. Every target session and every DDL statement uses `time_zone = '+00:00'`, so TIMESTAMP instants are preserved without any offset. DATETIME is zone-free. `mysql.time_zone` only affects the DATE_FORMAT fallback of 3.2.

### 3.5 PostgreSQL → PostgreSQL

**Reading.** The catalog is read per schema on a read-only source session (4.1) with `SET search_path = <schema>, pg_catalog`, always with the **pretty** variants (`pg_get_viewdef(oid, true)`, `pg_get_indexdef(oid, 0, true)`, `pg_get_constraintdef(oid, true)`, `pg_get_triggerdef(oid, true)`, `pg_get_expr(expr, rel, true)`), which leave names of the current schema unqualified. The target runs each schema's DDL under `SET search_path = <target schema>, pg_catalog`; this is how a schema rename works. `pg_get_functiondef` always qualifies the function's own name: the translator rewrites that header to the target schema. References inside function bodies and to other schemas are not rewritten (`RENAMED_REFS`, warning, when a renamed schema has views or functions). PostgreSQL cannot reference another database, so the MySQL qualifier problem does not exist here.

Never copied: system schemas (`pg_catalog`, `information_schema`, `pg_toast`, `pg_temp_*`); objects that belong to an extension (`pg_depend.deptype = 'e'`; `CREATE EXTENSION` creates them); indexes with `relispartition` (created through the parent's index); constraints with `conislocal = false` and triggers with `tgparentid <> 0` (inherited); internal triggers (`tgisinternal`, e.g. the FK `RI_ConstraintTrigger_*`); invalid indexes (`indisvalid = false`, `INVALID_INDEX_SKIPPED`, info).

**Source checks.**
- `SRC_RLS` (blocker): a table with `relrowsecurity` whose reading user is not superuser, has no BYPASSRLS, and is not the owner or the table has `relforcerowsecurity`. Such a read would silently return only the rows the policies allow; the source sessions also run `SET row_security = off` (4.1), as pg_dump does, so a missed case errors instead of filtering.
- `PG_SQL_ASCII_SOURCE` (blocker) when the source `server_encoding` is `SQL_ASCII` and `pg.source_encoding` is unset (SQL_ASCII does no conversion: EUC-KR/CP949 bytes fail the COPY into a UTF8 target or stay EUC-KR in a SQL_ASCII one). With `pg.source_encoding = euc-kr | latin1 | utf8`, the source session uses `client_encoding = SQL_ASCII` (no conversion) and core transcodes the raw CSV stream and the catalog text to UTF-8 with a streaming `encoding_rs` decoder (`utf8` only validates). CSV's special bytes (`,`, `"`, CR, LF) are ASCII and never CP949 trail bytes, so record boundaries stay intact. Invalid bytes become U+FFFD and are counted (`SourceStream.replaced`). This is the only PG path that alters the COPY bytes.
- `PG_LARGE_OBJECTS` (warning) when `oid`/`lo` columns exist and `pg_largeobject_metadata` is not empty: the copied numbers point at large objects that do not exist on the target.
- `PG_DB_ROLE_SETTINGS` (warning): `pg_db_role_setting` entries for the source database or the roles of copied objects (for example a `search_path` set on the database) are not copied; listed in detail.

**Target checks** (`query_meta`):
- The database exists and is reachable.
- `server_encoding` UTF8 or equal to the source, else `PG_ENCODING` (warning: characters it cannot hold fail the COPY). Both sessions use `client_encoding = UTF8` (except the SQL_ASCII source above), so the servers convert (a source in EUC_KR works).
- `has_database_privilege(db,'CREATE')`, `has_schema_privilege(s,'CREATE,USAGE')`; for existing tables `has_table_privilege(t,'INSERT')` (+ `'TRUNCATE'` for truncate) and ownership (`pg_has_role(relowner,'USAGE')`) for drop_recreate. Missing: `TGT_PRIVILEGE` (blocker; PG's is produced here, by copy-schema).
- Extensions: source `pg_extension` vs target `pg_available_extensions`; missing → `PG_EXTENSION_MISSING` (item blocker on the extension; dependants follow it). Version difference: info. Untrusted extensions need a superuser: `PG_EXTENSION_PRIVILEGE` (warning) when the target user is not one and `pg_available_extension_versions.trusted` is false.
- Collations used by columns and indexes that are not copied (system, ICU, libc ones) must exist in the target's `pg_collation` by name: `PG_COLLATION_MISSING` (item blocker; with `collation_fallback` a warning and the default).
- `PG_DEFAULT_COLLATION_DIFFERS` (warning) when `datcollate`, `datctype` or the locale provider (`datlocprovider`, 17's `builtin`) differ: ORDER BY, whether LIKE can use indexes (C vs non-C) and range-partition routing on text keys change. Text partition keys always get an explicit `COLLATE` (`pg_get_partkeydef` omits the default collation), and columns get one when the source default collation exists on the target as a named collation: `translate::pg::named_default_collation` maps a libc default (`C`, `POSIX`, or a `pg_catalog` collation whose name equals `datcollate` ignoring case and `-`, e.g. `en_US.UTF-8` → `pg_catalog."en_US.utf8"`), columns on the default collation (`attcollation` = 100) carry it, and `partkey_with_collations` inserts it after each key column or expression on the default collation (`pg_partitioned_table.partcollation` = 100), before an operator class. ICU and builtin defaults are not mapped (section 11).
- `lc_monetary` differing while a `money` column is copied: `PG_MONEY_LOCALE` (warning: COPY text of money depends on it).
- Roles (when `owners`/`grants`): missing → `PG_ROLE_MISSING` (warning; that statement is skipped).
- 17+ targets: `PG17_SEARCH_PATH` (warning) for each function without `search_path` in `proconfig` that is used by an expression or partial index (`pg_index.indexprs`/`indpred`) or a materialized view: PG17 runs CREATE INDEX, REFRESH, ANALYZE, CLUSTER, REINDEX and VACUUM with `search_path = pg_catalog, pg_temp`, so unqualified references in such functions fail (recommendation: `ALTER FUNCTION … SET search_path = …` on the source or after the copy).

**What is copied, in order** (`phase` / `order`; dependencies from a topological sort of `pg_depend`, phases as tie-breakers):

| phase | objects (statement source) |
|---|---|
| pre_data | `CREATE EXTENSION IF NOT EXISTS name WITH SCHEMA s`; `CREATE SCHEMA IF NOT EXISTS`; user collations (`CREATE COLLATION` from `pg_collation`: provider, locale / lc_collate / lc_ctype, deterministic); types: enum (`CREATE TYPE … AS ENUM`, labels by `enumsortorder`), domain (`format_type` + default + its **validated** checks; `convalidated = false` ones go to post_data), composite, range (`unsupported` with a custom subtype diff function); standalone sequences (`CREATE SEQUENCE` with type, increment, min, max, start, cache, cycle from `pg_sequence`); functions/procedures (`pg_get_functiondef`, target session `SET check_function_bodies = off`; one taking a table rowtype comes after that table; aggregates `unsupported`); tables: columns via `format_type`, `COLLATE`, defaults, NOT NULL (PG18 named NOT NULL constraints, `contype = 'n'`, as `CONSTRAINT n NOT NULL` on 18+ targets, plain NOT NULL below), identity `GENERATED {ALWAYS\|BY DEFAULT} AS IDENTITY (SEQUENCE NAME "s"."seq" …options)` as pg_dump writes it, `GENERATED ALWAYS AS (expr) STORED`, **validated** CHECK constraints (NOT VALID ones go to post_data: inside CREATE TABLE PostgreSQL would ignore NOT VALID and enforce them during COPY), `NOT ENFORCED` constraints (PG18) on 18+ targets, else `unsupported` + `DOWNGRADE`, `UNLOGGED`, reloptions, `PARTITION BY` (`pg_get_partkeydef` + explicit COLLATE); partitions `CREATE TABLE … PARTITION OF … FOR VALUES …` (`pg_get_expr(relpartbound)`, incl. `DEFAULT`); serial-owned sequences before their table, then `ALTER SEQUENCE … OWNED BY`; plain inheritance kept as pg_dump does: children `CREATE TABLE child (<local columns, attislocal>) INHERITS (parent)` with their local constraints (`conislocal`), and inherited columns whose default or NOT NULL differs from the (first) parent's get `ALTER TABLE ONLY child ALTER COLUMN c SET DEFAULT … | DROP DEFAULT | SET NOT NULL` after the CREATE (partitions likewise); column attributes `ALTER TABLE … ALTER COLUMN … SET STATISTICS` (attstattarget), `SET STORAGE` (non-default), `SET COMPRESSION` (14+ on both sides); `REPLICA IDENTITY` (non-default); views and materialized views with their full `reloptions` (`security_invoker`, `security_barrier`, `check_option`, storage options), materialized views `… WITH NO DATA`. Tablespaces are ignored (`PG_TABLESPACE_IGNORED`, info). |
| data | one item per ordinary table that is not a partition (inheritance parents and children each read `FROM ONLY`), and per partitioned **root** (rows routed by PG); extension configuration tables (`pg_extension.extconfig`, e.g. PostGIS `spatial_ref_sys`, pg_cron jobs): a data item reading only the rows matching `extcondition`, action `append` whatever the policy (CREATE EXTENSION created the table and its default rows; a key conflict fails the item; `EXT_CONFIG_ROWS`, info), as pg_dump does. |
| post_data | PRIMARY KEY / UNIQUE / EXCLUDE (`ALTER TABLE … ADD CONSTRAINT … <pg_get_constraintdef>`); other indexes (`pg_get_indexdef`, not constraint-backed); NOT VALID checks (`ALTER TABLE … ADD CONSTRAINT … CHECK (…) NOT VALID`) and domain checks (`ALTER DOMAIN … ADD CONSTRAINT … NOT VALID`); `ALTER TABLE … CLUSTER ON idx`; foreign keys after **all** tables are loaded (DEFERRABLE / INITIALLY DEFERRED kept); triggers last, so they never fire during the load, each followed by its `tgenabled` state (`D` → `ALTER TABLE … DISABLE TRIGGER t`, `R` → `ENABLE REPLICA TRIGGER t`, `A` → `ENABLE ALWAYS TRIGGER t`); RLS (`ENABLE/FORCE ROW LEVEL SECURITY`, `CREATE POLICY` from `pg_policy`). |
| finalize | `SequenceValue` items (rule below); `REFRESH MATERIALIZED VIEW`; `COMMENT ON …` (text via `source::postgres::quote_literal`); `ALTER … OWNER TO` / `GRANT` from `aclexplode` (schemas, tables, views, sequences, functions, and types / domains from `typacl` as `GRANT USAGE ON TYPE | DOMAIN`); `ANALYZE`. |

With `indexes_after_data = false`, constraints and indexes move to pre_data (still before FKs, NOT VALID checks and triggers). Unsupported objects (casts, operator classes and families, text search configurations and dictionaries, rules, aggregates, statistics objects, FDW objects, publications, event triggers) get `UNSUPPORTED_OBJECT` (warning).

**Sequence values never move backwards.** A `SequenceValue` item resolves its target sequence with the target's `pg_get_serial_sequence` (identity and serial columns) or by name (standalone). Its value: for a sequence this run created, the source `last_value` / `is_called` read at finalize after its table loaded; for a sequence that already existed in the target, and for the sequences of append or truncate tables, `GREATEST(source value, target last_value when is_called, max(owning column))` (LEAST for descending sequences), then `setval(seq, v, true)`. The plan item's `target_sql` is the template `SELECT pg_catalog.setval('"s"."q"', <source value>, <is_called>)` or, for the floor rule, `… GREATEST(<source value>, <target value>) …`; its detail shows both values as read at plan time. When reading the target floor fails at finalize (the target sequence does not exist or is unreadable), the item gets `SEQUENCE_FLOOR_UNREADABLE` (info) and uses the source value without a floor check. MySQL's AutoIncrement already only raises.

**SECURITY DEFINER.** With `owners = false`, SECURITY DEFINER functions become owned by the copy account and run with its rights: `PG_SECDEF_OWNER` (warning) lists them.

**Version notes.** DDL from a 15 server's `pg_get_*def` is valid on 16, 17 and 18, except the PG17 restricted search_path above. `adminpack` is gone in 17 (the extension check catches it). PG18 sources add `contype = 'n'` and NOT ENFORCED constraints (handled above).

### 3.6 SQLite → SQLite

- DDL verbatim from ``SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite\_%' ESCAPE '\'`` (an unescaped `_` would hide user tables such as `sqlitex`). Order: tables (pre_data), data, indexes with `sql IS NOT NULL` (post_data), views, triggers.
- Virtual tables (`sql LIKE 'CREATE VIRTUAL TABLE%'`) and their shadow tables (`<vt>_content`, `_data`, `_idx`, `_docsize`, `_config`, `_segments`, `_segdir`, `_stat`, `_node`, `_parent`, `_rowid`) are `unsupported` (`UNSUPPORTED_OBJECT`, warning), so nothing is copied twice.
- Truncate is `DELETE FROM "t"`. `sqlite_sequence` (no unique key, so `INSERT OR REPLACE` would add a duplicate row) is set in finalize with bound parameters: `UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = ?2`, then `INSERT INTO sqlite_sequence(name, seq) SELECT ?2, ?1 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = ?2)`.
- Effective `table_concurrency` is 1 (writers on one file serialise on its write lock).
- Values follow the `source/sqlite.rs` affinity mapping; mixed storage classes in one column can be lossy (documented limit of the test pair).

### 3.7 Existing objects, selection and actions

The policy of a table is `existing_overrides[item_id]` when set (UI: the 작업 chip menu of the plan table; CLI `--existing-for [s.]t=<policy>`), else the global `existing` (default **skip**).

| target state | skip (default) | truncate | drop_recreate | append |
|---|---|---|---|---|
| table missing | `create` + load | `create` + load | `create` + load | `create` + load |
| table exists | `skip_existing`: no DDL, no data; its indexes/FKs/triggers/sequence values skipped too | `truncate` + load; existing indexes kept, missing ones not created | drop (rules below), then create + load | load into it; columns matched by target name; a missing source column for a NOT NULL target column without default, or a source column missing in the target → `APPEND_COLUMNS_MISMATCH` (item blocker) |
| other object exists (view, function, type, sequence, index, trigger) | skipped | skipped | dropped (rules below) and recreated | skipped |

- **Row presence.** For every existing table the plan would skip, truncate, drop or append into, the planner runs an exact, cheap probe: PG `SELECT EXISTS (SELECT 1 FROM ONLY t)`, MySQL/SQLite `SELECT 1 FROM t LIMIT 1`. `PlanItem.target_has_rows` and `target_est_rows` carry the result; the plan table shows "있음 · 데이터 있음" / "있음 · 비어 있음". Catalog estimates are never used for this (PG `reltuples` is -1 or 0 before the first ANALYZE; InnoDB `TABLE_ROWS` misses full tables).
- **Destructive actions.** `truncate` / `drop_recreate` on a table with `target_has_rows = true`: `DESTRUCTIVE_TRUNCATE` / `DESTRUCTIVE_DROP` (danger). `append`/`truncate` into a table with triggers: `TARGET_TRIGGERS` (warning: they fire during the load). A new table's FK to a skipped existing table: `FK_TO_SKIPPED` (warning: validation may fail). PG truncate of a table with rows: `TARGET_LOCKED_DURING_LOAD` (warning: TRUNCATE inside the load transaction holds ACCESS EXCLUSIVE until the load commits, blocking every application query on it).
- **Truncate.** PG: `TRUNCATE ONLY t` inside the load transaction (partitioned root: `TRUNCATE t`; atomic with the load). When target FKs reference truncated tables, each FK-connected group of truncate tables is emptied by its own `TruncateGroup` item (`TRUNCATE ONLY a, ONLY b …`, order 0 of the data phase; the group's data items depend on it; `TRUNCATE_GROUPED`, info: not atomic with the load); a referencing table outside the group is `TRUNCATE_FK_REFERENCED` (item blocker). MySQL: `TRUNCATE TABLE` (not transactional). SQLite: `DELETE FROM "t"`.
- **Drop.** Every drop is `RESTRICT` on PG (never CASCADE) and plain on MySQL. For every drop_recreate item the planner lists the target's dependants: PG from `pg_depend` (views and materialized views through `pg_rewrite`, FKs of other tables through `pg_constraint.confrelid`, policies, functions and columns using the rowtype or type); MySQL from `information_schema.KEY_COLUMN_USAGE` (`REFERENCED_TABLE_SCHEMA`/`_NAME`, any database) and `information_schema.VIEWS` whose definition names the table. Dependants that are themselves drop_recreate items are dropped first, in reverse dependency order (FKs between dropped tables first by `ALTER TABLE … DROP CONSTRAINT` / `DROP FOREIGN KEY`). Any other dependant is `DROP_HAS_DEPENDANTS` (item blocker; names in detail; resolve by excluding the item or choosing another policy). Schemas, extensions and databases are never dropped (the allow-list of 5.2 rejects those statements). Each drop is recorded in `CopyResult.objects`.
- **Load order and FK checks into existing tables.** PG enforces existing target FKs during COPY, so data items of existing tables depend on their target-FK parents' data items (parents first, also with concurrency). MySQL does the same: loads into existing tables that have target FKs keep `foreign_key_checks = 1` (`TableDataSpec.fk_checks`) and go parents first. A cycle is `FK_CYCLE` (warning: recommendation drop_recreate); on MySQL that group loads with `foreign_key_checks = 0` and its FKs are orphan-checked in verification (5.6). New tables load with `foreign_key_checks = 0`; their FKs come in post_data.
- **Skipped tables.** When at least one selected table is `skip_existing`: `TABLES_SKIPPED_EXISTING` (warning; detail: the count and how many are empty according to `target_has_rows`; recommendation "empty tables: append or data only" / "빈 테이블은 이어 붙이기 또는 데이터만"). A common flow creates the schema with the application's migrations first; with the default policy such tables would otherwise be skipped silently. `SKIPPED_DIFFERS` (warning) when a skipped table's target columns differ from the translated ones (names or types), or the target is empty while the source estimate is above 0.
- `content: schema_only` turns every load off. `data_only` turns DDL off: a missing table is `TABLE_MISSING` (item blocker); if every selected table is then skipped, `DATA_ONLY_ALL_SKIPPED` (warning: choose truncate or append).
- **Selection `tables`.** Only those tables are loaded. Objects they need (schemas, types, domains, collations, sequences, extensions, functions used by defaults) are included. Views, triggers, FKs and policies that reference an unselected table become `excluded` with `DEPENDENCY_NOT_SELECTED` (info).
- **Excluded items** (`options.exclude`) and their dependants get action `excluded`.
- **`objects.sequences`** controls only the `SequenceValue` and `AutoIncrement` items. Sequences that serial, identity or default columns need are always created. With it off: `SEQUENCES_NOT_SET` (warning: the application's next INSERT on the target collides with copied keys).

## 4. Data paths (owner copy-data)

### 4.1 Common pipeline

Per table worker: one source `LiveConnection` (application_name `bulkdb-<job>-s<n>`) runs `Source::stream(Query { sql: spec.source_sql, output, max_batch_bytes: Some(BATCH_BYTES / concurrency) }, session, cancel)` → a byte-budgeted channel → one `TargetSession` (`bulkdb-<job>-t<n>`) runs `load`. Post-data work of a table (index ALTERs, FK ALTERs) runs in the same pool and counts against `table_concurrency`.

**Memory** (whole job, any concurrency c):
- Readers: each builds at most one batch and queues at most `READER_CHANNEL_DEPTH` (2) more inside its driver, each ≤ `max_batch_bytes` = `BATCH_BYTES / c` → ≤ 24 MiB in all.
- In flight reader → writer: a `tokio::sync::Semaphore` of **16 MiB**. A reader acquires `min(batch bytes, 16 MiB / c)` permits (measured by the batcher's own byte counter, which tracks buffer growth that `get_array_memory_size` misses) and sends `Budgeted { item, rows, permit }` through a channel of depth 1; the writer drops the permit after encoding or forwarding the batch. `rows` feeds `rows_written`, checkpoints and progress (RawCsv carries `Batch::RawCsv.rows`).
- Writers: at most two encoded **pieces** each (one being filled, one queued to the server) of ≤ `piece_mb = min(batch_mb, 8 / c)` MiB → ≤ 16 MiB in all.
- Total ≤ 56 MiB of data buffers, under the 100 MB RSS rule; an RSS test pins it (section 9). No `Vec<Row>`, no `collect()` of results: writers encode column-wise straight from the `RecordBatch` arrays or pass raw bytes on.

**Source sessions.**
- Every `source_sql` passes `sqlguard::check_for(sql, dialect)` as one read-only SELECT before it runs (`INTERNAL` otherwise). Data sessions get the merged export settings (`export::merge_session`) without a statement timeout.
- **Plan / catalog session** (`bulkdb-copyplan`, or `bulkdb-<job>-m` for a job, kept open by the job until finalize): these **deparse settings** ensure `pg_get_expr`, `pg_get_constraintdef`, `pg_get_viewdef` and partition bounds print in the format the target replays, and TIMESTAMP column defaults from `SHOW CREATE TABLE` are in UTC. PG: `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY; SET row_security = off; SET DateStyle = ISO, YMD; SET IntervalStyle = postgres; SET extra_float_digits = 3; SET TimeZone = 'UTC'`. MySQL (5.6.5+, not MariaDB): `SET SESSION TRANSACTION READ ONLY`; 4.1.3+: `SET time_zone = '+00:00'`. A failing setting fails the session. Every meta query passes `sqlguard::split_statements(sql).len() == 1`. Source meta and data sessions also clear server/role/database timeouts they might inherit: PG 9.6+: `idle_in_transaction_session_timeout = 0; statement_timeout = 0`; MySQL 5.7.8+: `max_execution_time = 0`; MariaDB 10.1.1+: `max_statement_time = 0`.
- PG data sessions: `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY; SET row_security = off; SET DateStyle = ISO, YMD; SET IntervalStyle = postgres; SET extra_float_digits = 3; SET bytea_output = hex; SET TimeZone = 'UTC'; SET client_encoding = 'UTF8'` (`SQL_ASCII` for the transcoding case of 3.5), through the downcast `PgDatabase.client`.
- MySQL data sessions open with `conn.database = endpoint.database` (the `TextPlan` depends on the connected database's `character_set_database`); the existing `SET SESSION TRANSACTION READ ONLY` (5.6.5+) stays; `SET time_zone = '+00:00'` on 4.1.3+ through `meta_exec`.
- SQLite: the existing read-only open.

**Target sessions.**
- The target plan session (used by `copy_plan` and by a job until it starts writing) is read-only like the source's: PG `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY`, MySQL `SET SESSION TRANSACTION READ ONLY` (5.6.5+), SQLite opened without write access (3.6 / 4.4).
- PG write sessions: `SET statement_timeout = 0; SET idle_in_transaction_session_timeout = 0; SET lock_timeout = '<n>s'` (n = `target_lock_timeout_s`, default 5) plus the 4.2 settings. MySQL: the prelude of 4.3, with `lock_wait_timeout = <n>`.
- A lock wait over the limit fails that item with `TARGET_LOCK_TIMEOUT` ("the target table is in use; try again later"); the run stays resumable. Waiting forever in the lock queue would block every application query queued behind an ACCESS EXCLUSIVE or metadata lock.
- The lease session of 5.1 holds the target lease for the job's lifetime.

**Session totals** (`CopyEffective`): source 1 (plan/meta) + c (+1 with checksum verification); target 2 (plan/meta, lease) + c (+1 with checksum). The plan facts line and the concurrency help text show them.

**Consistent snapshot.**
- PG: the job's meta session runs `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SELECT pg_export_snapshot()` and stays in that transaction until the last read ends (committed with `COMMIT` after the last worker finishes); each worker runs `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT '<id>'` before its COPY and `COMMIT` after (pg_dump's method; tokio-postgres can do it). If the export fails (some standbys), there is no snapshot.
- MySQL: with `table_concurrency = 1`, a 4.1.8+ source and every selected table on InnoDB, the single worker session explicitly sets `REPEATABLE READ` isolation and then runs `START TRANSACTION WITH CONSISTENT SNAPSHOT`, reading all tables in that transaction. Concurrency > 1 loses the snapshot even if each worker sets REPEATABLE READ (each transaction gets its own snapshot).
- Otherwise each table is read at its own time and a live source can yield cross-table inconsistencies that FK validation does not catch (missing child rows, stale totals): `SRC_NOT_SNAPSHOT` (warning, needs an acknowledgement). If the snapshot is lost during a multi-table run (the session is interrupted and reconnected), `SRC_NOT_SNAPSHOT` may also be emitted at runtime through the run-issues channel. `CopyEffective.consistent_snapshot` says which case applies at plan time. A long snapshot holds back PG vacuum and InnoDB purge (`SRC_LONG_SNAPSHOT`, 5.3).

### 4.2 PostgreSQL → PostgreSQL: raw CSV pass-through

- **Source:** `StreamOutput::RawCsv(CsvFormat { delimiter: b',', header: false, null_as: "".into(), crlf: false })` over `SELECT <cols> FROM ONLY "s"."t"` (partitioned root: without `ONLY`; extension config tables: `… <extcondition>`; generated columns left out of both column lists). `Batch::RawCsv` bytes are never decoded, except by the SQL_ASCII transcoder of 3.5.
- **Target:** `BEGIN` → (truncate, 3.7) → `COPY "s"."t" ("c1", …) FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '', QUOTE '"', ESCAPE '"')` via `client.copy_in` → `COMMIT`. The bytes go into the `CopyInSink` unchanged. The target session runs the same `SET` statements as the source data session (except READ ONLY and row_security) plus the 4.1 write settings.
- **Exactness:** COPY CSV writes NULL as an unquoted empty field and an empty string as `""`, so the round trip is exact. COPY FROM writes supplied values into identity columns (like `OVERRIDING SYSTEM VALUE`) and fires target triggers, which is why BulkDB creates triggers after data.
- **Atomicity:** a table load is one transaction; failure or cancel rolls it back (dropping the sink sends CopyFail).
- **No binary COPY**, not even for the same major version (not portable across majors; a possible later optimisation).

### 4.3 MySQL → MySQL: Arrow → LOAD DATA LOCAL INFILE, or INSERT batches

**Target connection and session.** Each writer opens **one** dedicated `mysql_async::Conn` from `OptsBuilder::from_opts(MySqlDatabase::opts().clone()).db_name(None::<String>).local_infile_handler(Some(TokenHandler)).init(prelude)`. Every statement of that writer (session SETs, DDL, TRUNCATE, loads, SHOW WARNINGS, probes) runs on it, and all SQL is fully qualified (`` `db`.`t` ``), so a target database that the `Database` item creates never breaks the connect. The target `LiveConnection` only keeps the tunnel lease and runs `KILL QUERY` from a side connection for `cancel_running`. `init` runs the prelude on every connect and reconnect:
1. `SET NAMES utf8mb4` (utf8 before 5.5.3).
2. `SET SESSION sql_mode = '<m>'`, m = the target's `@@GLOBAL.sql_mode` − `ANSI_QUOTES` − `NO_BACKSLASH_ESCAPES` + `STRICT_ALL_TABLES` + `NO_ENGINE_SUBSTITUTION` + `NO_AUTO_VALUE_ON_ZERO` (a 0 id stays 0); under `keep` also − `NO_ZERO_DATE` − `NO_ZERO_IN_DATE` + `ALLOW_INVALID_DATES` (strict mode accepts zero dates once those flags are gone). 5.0-5.6 targets have a non-strict global mode by default; this makes every bad value an error on the INSERT path.
3. `SET SESSION time_zone = '+00:00'`.
4. `SET SESSION foreign_key_checks = 0` (1 for the loads of 3.7 that keep FK checks).
5. `SET SESSION lock_wait_timeout = <n>` (5.5.3+), `net_read_timeout = 600`, `net_write_timeout = 600`, `wait_timeout = 28800`. `unique_checks` is never turned off.

Before each INSERT batch and each LOAD chunk the writer asserts `SELECT @@session.time_zone, @@session.sql_mode, @@session.character_set_client` against the expected values; a mismatch is `INTERNAL` "target session lost its settings" (a missing time_zone would shift every TIMESTAMP silently). The prelude is part of `CopyPlan.session_sql.target`.

**Input.** Arrow batches with text already decoded to UTF-8 (EUC-KR and latin1-held CP949 decoded by `TextPlan` / `encoding.rs`; invalid bytes are U+FFFD). Temporal columns arrive as text, epochs or YEAR numbers (3.2). The pure module `copy::data::mysql_encode` (shared with `RecordingTarget`) holds `normalize_temporal(value, kind, read, actions) -> Normalised` (epoch → UTC text, CAST text, compact 4.0 forms through `mysql_common::parse_compact_timestamp` for the fallback, YEAR numbers, chrono calendar validation, the zero-date and invalid-date actions of 3.4, counting into `value_fixes`), the LOAD DATA field encoder and the INSERT literal encoder. Arrow kinds from the existing MySQL readers: integers `Int64` (BIGINT UNSIGNED and wide DECIMAL as `Utf8`), BIT `Int64` (reinterpreted as u64), DECIMAL `Decimal128`, FLOAT/DOUBLE `Float64`, text/ENUM/SET/JSON `Utf8`, binary `Binary`.

**unmappable_chars** = U+FFFD produced by decoding a non-UTF-8 source (`SourceStream.replaced`; a UTF-8 source's own U+FFFD is not counted) + characters the writer replaced by `?` on utf8 (3-byte) targets. With `on_unmappable: fail` the first one fails the table (`UNMAPPABLE_CHARS`); with `replace` (default) the table ends "done with problems" (5.6).

**Method.** `MysqlLoadData` when the target's `local_infile` = ON, else `MysqlInsert` with `LOCAL_INFILE_OFF` (info; 8.0+ ships with it OFF). BulkDB never changes server variables.

**Chunks.** One LOAD statement (or a run of INSERT statements) per chunk of ≤ 500,000 rows and ≤ `chunk_bytes`. `chunk_bytes` = 64 MiB, capped at half of the smallest of `group_replication_transaction_size_limit` (150,000,000 by default on 8.0 InnoDB Cluster), `wsrep_max_ws_size` (Galera), `max_binlog_cache_size` when set; rows are capped at half of `wsrep_max_ws_rows` when set. The effective value is in `CopyEffective.chunk_bytes` and the facts line. InnoDB targets run each chunk in `START TRANSACTION` … `COMMIT` (`ROLLBACK` on error, warning or cancel), so only whole chunks persist; MyISAM keeps the rows of an interrupted chunk. Checkpoints count committed rows. When no piece has arrived for 10 s (a stalled or slow source, a MyISAM lock wait), the writer ends the chunk at the last row boundary and commits; the next piece opens a new chunk, so `net_read_timeout` never aborts a load.

**LOAD DATA path.**
- `Conn::set_infile_handler` is **not** used: the per-connection handler never sees the requested file name. `TokenHandler: GlobalHandler` holds `Mutex<Option<(String, InfileData)>>`; the writer arms it before each statement with token `bulkdb-<job>-<n>-<chunk>` and an `InfileData` fed by a bounded `mpsc` (depth 1) of encoded pieces (`Bytes`, ≤ `piece_mb` each). `handle(file_name)` returns the armed stream only when `file_name` equals the token, else a stream yielding an error. No disk file is ever read, so a server cannot request a local file.
- Pieces always end on a row boundary; a single row larger than `piece_mb` gets its own piece. A stream ended early (error or cancel) therefore never leaves a truncated row in MyISAM.
- Statement: ``LOAD DATA LOCAL INFILE 'bulkdb-…' INTO TABLE `db`.`t` CHARACTER SET <cs> FIELDS TERMINATED BY '\t' ESCAPED BY '\\' LINES TERMINATED BY '\n' (`c1`, @v2, `c3`) SET `c2` = UNHEX(@v2)`` with `<cs>` the session's `SET NAMES` charset; the server converts into the column charset. Targets before 5.0.38 / 5.1.17 have no `CHARACTER SET` clause: run `SET SESSION character_set_database = <cs>` first.
- Binary, varbinary, blobs, geometry: hex text + `SET col = UNHEX(@v)`. BIT: decimal text + `SET col = CAST(@v AS UNSIGNED)`. MySQL string functions return NULL with warning 1301 when the result exceeds `max_allowed_packet`, so the writer checks each binary value's length against the target's `max_allowed_packet` − 1 KiB before sending and fails the table with `ROW_TOO_LARGE` (hint: raise `max_allowed_packet`).
- Field encoding: NULL = `\N`. In text: `\` → `\\`, TAB → `\t`, LF → `\n`, CR → `\r`, NUL → `\0`, 0x1A → `\Z`; every other byte raw UTF-8. Empty string = empty field. TAB between fields, LF after rows. Values: integers decimal; Float64 by Rust `Display` (shortest round trip); Decimal128 exact at its scale; Utf8 escaped; temporal normalised UTC text; JSON text.
- **Warnings.** With LOCAL the server behaves as IGNORE: truncated strings, out-of-range numbers, invalid dates (stored as 0000-00-00), `\N` into NOT NULL (stored as the implicit default), oversized function results (NULL) and 'Incorrect string value' all become warnings, and duplicates are skipped. After each LOAD statement and before its COMMIT, the writer reads the OK info `Records: N Deleted: 0 Skipped: S Warnings: W`. When `S + W > 0` it runs `SHOW WARNINGS LIMIT 10` into `warning_samples` (redacted, 5.2), rolls the chunk back on InnoDB, stops the table and fails it with `DATA_WARNINGS` (or `ROWS_SKIPPED` when S > 0); `data_warnings += W`.

**INSERT path.** ``INSERT INTO `db`.`t` (`c1`, …) VALUES (…),(…)`` inside the chunk transaction.
- Literals: strings `mysql_common::quote_str(s, lex)` with `lex.backslash_escapes = true` (BulkDB's session `sql_mode` has no `NO_BACKSLASH_ESCAPES`; the connection is utf8/utf8mb4, so no multibyte backslash issue); binary `X'…'`; BIT an unsigned integer; NULL `NULL`; numbers bare; temporal quoted normalised text.
- A statement is ≤ `min(max_allowed_packet − 64 KiB, piece_mb MiB)` bytes and ≤ `batch_rows` rows. A single row over the packet is `ROW_TOO_LARGE`.
- Strict mode turns bad values into errors, which fail the table. The OK packet's warning count is read after every statement; any warning runs `SHOW WARNINGS LIMIT 10`, rolls the chunk back and fails the table with `DATA_WARNINGS`.

**DDL statements.** After every DDL statement the writer runs `SHOW WARNINGS`. Deprecation notes (1287, 1681, 3719, 3778) and duplicate-index 1831 annotate the item (info); every other Warning or Error row fails it (`DDL_WARNING`, redacted text in detail). This catches silent substitutions such as an oversized VARCHAR turned into TEXT. `TargetFacts.vars` include `innodb_file_per_table`, `innodb_strict_mode` and `innodb_page_size` for the 3.3 rules.

### 4.4 SQLite → SQLite

`SqliteTarget` opens its own `rusqlite::Connection` (not through `source::open`, whose `SqliteDatabase::connect` refuses a missing file), on a blocking thread. For planning, meta reads and dry runs it opens read-only without `OPEN_CREATE`, and a missing target file is an empty database (no file is created, so a dry run leaves no trace). Only an executing job opens it with `OPEN_CREATE`. Loading: `BEGIN`, a prepared `INSERT INTO "t" ("c1", …) VALUES (?, …)` bound column-wise from the Arrow arrays (no row structs), `COMMIT` every `batch_rows`; a table failure rolls back only the open batch, so the table is reloaded on resume. Bindings: Int64 → integer, Float64 → real, Utf8 → text, Binary → blob, Decimal/Date/Timestamp → ISO text. Free space: `fs4::available_space` of the target's directory; below est_bytes × 1.2 → `DISK_LOW` (warning).

## 5. Execution (owner copy-data)

### 5.1 Job flow

`copy_start(req, planId, ackKeys)` creates the job (`create_copy`) and spawns `copy::exec::run_copy_job`:
1. **pending**.
2. **preflight**: claim the run (5.5; `RUN_IN_USE` / `TARGET_IN_USE` inside the claim); open the endpoints; take the target lease (below); `build_plan`; `preflight::finalize`. Blockers → **rejected** (JobDone error `INVALID` listing the codes; `copy_status` holds the plan).
3. **warned** when an ack key is missing (section 2) or `plan_id ≠ planId`, which adds `PLAN_CHANGED` (warning) whose detail lists the item ids whose action or `target_sql` changed against the reviewed plan (`CopyState` keeps the last 16 plans of the process by `plan_id`; when the old plan is unknown the detail says so). `JobEntry.preflight` gets a `PreflightReport` whose `warnings` are the issues still needing an acknowledgement, so the generic `job_resume(jobId, ackKeys)` continues the job. The UI renders a warned copy job in the CopyWizard plan step, never in the export PreflightSheet (section 7).
4. **running**: pre_data serially in `order`; data with up to `table_concurrency` tables at once, honouring `depends_on`; post_data per table in the same pool as soon as that table's data is done, except FKs, which wait for every table; then finalize, which reads the runtime values (`read_finalize_values`, `read_target_floor`) on the job's meta session and builds the statements with `finalize_sql`.
5. **finishing**: verification and the result.
6. **done**, **failed** or **cancelled**.

One failing item does not stop the others; its dependants become `skipped` with `DEPENDENCY_FAILED`. Outcomes are defined in 5.6; the `CopyResult` is always stored and `resumable` is true unless the run was a dry run.

**Target lease.** A run must be the only BulkDB writer of its target database, across windows, processes and machines. Before planning, the job opens a dedicated lease session on the target and takes: PG `SELECT pg_try_advisory_lock(hashtext('bulkdb-copy'))` (advisory locks are per database); MySQL `SELECT GET_LOCK('bulkdb-copy:<first 32 hex digits of SHA-256 of the database name>', 0)` (lock names are limited to 64 characters; the session holds no other lock, which also suits servers before 5.7.5 that allow one lock per session); SQLite `fs4` `try_lock_exclusive` on `<target file>.bulkdb-lock`. An in-process registry (`CopyState`) and the run claim of 5.5 cover the same database for this machine. When the lease is not available: `TARGET_IN_USE` (blocker) naming the other job, run or instance when known (registry, `copy_runs`). Closing the lease session at the end of the job releases it. Dry runs take it too. `SOURCE_BEING_WRITTEN` (warning) when a live copy job of this machine (registry or an in-use run) targets this job's source server and database. `copy_plan` reports `TARGET_IN_USE` from the registry and `copy_runs` without taking the lease.

**Dry run** (`dry_run: true`): plan; run each `source_sql` as `SELECT * FROM (<sql>) q WHERE 1=0` (MySQL 4.0, no derived tables: `<sql> LIMIT 0`) to prove the SELECTs and the type mapping.
- PG targets also execute the pre_data and post_data DDL of `create`/`run` items (CREATE, ALTER, COMMENT, GRANT) in one transaction, with a `SAVEPOINT` per item (`ROLLBACK TO SAVEPOINT` on its error) and `ROLLBACK` at the end. They never execute finalize items (`setval` and `AUTO_INCREMENT` change sequences outside the transaction; REFRESH and ANALYZE only add load), data, TRUNCATE or DROP statements. The statement guard enforces this with a separate dry-run allow-list (5.2).
- MySQL and SQLite targets execute nothing; only the source SELECTs and the type mapping are checked. The UI and CLI say so.
- Items end `planned`; the result and the run say `dryRun: true`; dry runs are never resumable. An item that would change an existing table (truncate, drop, append into an existing table) is not executed even on PG and gets `DRY_RUN_NOT_CHECKED` (info) on its item. After a successful dry run the UI offers [실제로 복사] (re-plan with `dry_run` false).

### 5.2 Statement execution

Only `plan.items[].target_sql`, the generated load statements, `plan.session_sql` and the lease statements run on a target, and every one is in the plan (data templates elided). Each must be exactly one statement, `sqlguard::split_statements_compound(sql, dialect).len() == 1` (the family's lexing rules, MySQL with backslash escapes; in a `CREATE … TRIGGER | FUNCTION | PROCEDURE | EVENT` statement a `;` inside its `BEGIN … END` body, e.g. SQLite and MySQL trigger bodies or PostgreSQL `BEGIN ATOMIC`, does not end the statement), and start with a keyword allowed for its item kind and family. The executor classifies a data item's statements with `translate::item_sql_parts` (DDL in the item's phase, the truncate right before the load, the load template never executed; MyISAM `DISABLE KEYS` right before and `ENABLE KEYS` right after the load). `exec::executed_statements` / `exec::refused_statements` replay a plan through the same checks; the planner tests of every family assert that real plans have no refused statement:

| family | allowed |
|---|---|
| all | `CREATE …` (not `CREATE DATABASE` except the MySQL `Database` item), `ALTER …`, `SET` (session scope only), `BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` / `RELEASE` |
| PG | `DROP TABLE\|VIEW\|MATERIALIZED VIEW\|FUNCTION\|PROCEDURE\|TYPE\|DOMAIN\|SEQUENCE\|INDEX\|TRIGGER\|POLICY\|COLLATION … RESTRICT` (drop_recreate items only), `TRUNCATE`, `COPY … FROM STDIN`, `COMMENT ON`, `GRANT`, `SELECT pg_catalog.setval(`, `SELECT pg_try_advisory_lock(`, `REFRESH MATERIALIZED VIEW`, `ANALYZE` |
| MySQL | `DROP TABLE\|VIEW\|TRIGGER` (drop_recreate items only), `TRUNCATE TABLE`, `CREATE DATABASE` (Database item), ``LOAD DATA LOCAL INFILE 'bulkdb-`` , `INSERT INTO`, `SELECT GET_LOCK(`, `ANALYZE TABLE` |
| SQLite | `DELETE FROM "t"` without WHERE (truncate items only), `UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = ?2`, `INSERT INTO sqlite_sequence(name, seq) SELECT ?2, ?1 WHERE NOT EXISTS (…)`, `INSERT INTO "t"` (prepared, bound), `ANALYZE` |
| dry run (PG) | `CREATE`, `ALTER`, `COMMENT ON`, `GRANT`, `SET`, `BEGIN`, `SAVEPOINT`, `ROLLBACK [TO SAVEPOINT]`, `RELEASE` only |

Always rejected, with unit tests per family: `SET GLOBAL`, `SET PERSIST`, `SET PERSIST_ONLY`, `SET @@global.`, `DROP DATABASE`, `DROP SCHEMA`, `DROP EXTENSION`, `DELETE` and `UPDATE` other than the SQLite shapes above, `KILL` (only `cancel_running` issues it, on its side connection), and anything with more than one statement. Target reads (`query_meta`, `count_rows`, row-presence probes, orphan probes, `SHOW WARNINGS`, `SHOW GRANTS`, `SHOW CREATE TABLE`) go through `sqlguard::check_for` as single read-only statements.

Statements are logged at debug through `logging::mask_secrets`; data is never logged. **Redaction:** `copy::redact_values(msg)` replaces data values in server messages with '…' before anything leaves the writer (errors, `warning_samples`, issue details, `copy_items.error`, `result_json`, logs): MySQL single-quoted literals except key and column names in known message shapes ("Duplicate entry '…' for key 'k'", "Incorrect string value: '…' for column 'c' at row 5"), PG `Key (cols)=(…)` value lists and the value part of `CONTEXT: COPY t, line 5, column c: "…"`, keeping the code, column and line. Unit tests use real message samples.

### 5.3 Preflight (`copy/preflight.rs`, issues added to the plan)

**env** = computed from live state, excluded from `plan_id` (section 2).

| code | severity | env | condition |
|---|---|---|---|
| `TARGET_NOT_WRITABLE` | blocker | env | target `Connection.write_allowed != Some(true)` |
| `PRODUCTION_CONFIRM` | plan: info "실행할 때 대상 이름 입력이 필요합니다"; start/resume: blocker when the name does not match | env | target tag `production` |
| `SAME_DATABASE` | blocker | | same server (identity below) and same database name, whatever the schemas; the same connection with the same database always |
| `SAME_SERVER_UNSURE` | danger | | MySQL < 5.6: only `@@hostname:@@port` match (start time or datadir differ or are unreadable) and same database name |
| `SOURCE_IS_REPLICA` | danger | | PG source `pg_is_in_recovery()` with `system_identifier` unreadable; MySQL source with unreadable replica status and `read_only = ON`. Message: "If the target is this replica's primary, the source data changes." |
| `FAMILY_MISMATCH` / `FAMILY_UNSUPPORTED` | blocker | | drivers differ / not postgres, mysql, sqlite |
| `WORK_MEM_HIGH`, `CONCURRENT_SESSION` | as in export | env | the export preflight's source checks (`preflight::check_work_mem`; `preflight::concurrent_sessions`, the session check of `preflight::run`, over the source tables of the plan's load items, run on the plan / meta session) on the merged source session. `WORK_MEM_HIGH` keeps export's severity (danger) and is acknowledged, not a blocker; a session listing that fails is `SESSIONS_UNKNOWN` (info, env) |
| `SRC_MYISAM_LOCK` | warning | env | MySQL source table on MyISAM > 1 GB: its writers wait (table lock) while it is read |
| `SRC_LONG_SNAPSHOT` | info | env | PG source table > 10 GB, or > 10 GB read under one snapshot (vacuum held back); MySQL InnoDB source table > 10 GB or a snapshot over > 10 GB (purge held back) |
| `SRC_NOT_SNAPSHOT` | warning | | no consistent snapshot (4.1) |
| `DISK_LOW` | warning | env | SQLite target only; PG/MySQL show `est_bytes` (table + index sizes) instead |
| `RUN_IN_USE` | blocker | env | resume of a run that is in use (5.5) |
| `TARGET_IN_USE` | blocker | env | the target lease or the run claim is held by another job (5.1, 5.5) |
| `SOURCE_BEING_WRITTEN` | warning | env | a live copy job of this machine targets this job's source (5.1) |
| `RESUME_TARGET_CHANGED_SERVER` / `RESUME_SOURCE_CHANGED_SERVER` | blocker / warning | | resume: identity or database differs from the stored one (5.5) |
| `TGT_PRIVILEGE` (MySQL) | blocker | | below |
| `BLOB_PACKET_LIMIT`, `TGT_REPLICATION_LAG_RISK` | warning / info | | 3.3 |
| `UNKNOWN_ITEM` | danger, acknowledged | env | one or more ids in `options.exclude` or `existing_overrides` match no item of this plan and have no effect (ids from a previous plan that is now out of date) |
| `ENUM_ERROR_VALUE` | warning / info | env | MySQL: one or more tables have ENUM columns holding the error value `''` that the strict target refuses; severity warning when `enum_error: fail` (table will fail at load), info when `enum_error: null`; per-item with the table id (3.3) |

**Server identity** (`preflight::identity(meta) -> ServerIdentity` and `same_server(a, b) -> Same | Unsure | Different`, pure and unit-tested with canned facts for every rule):
- PG: `system_identifier` from `pg_control_system()` when readable. Equal on both sides ⇒ same cluster, whatever the start time or port (a physical standby and its primary share it). Otherwise the key is `pg_postmaster_start_time()` + `current_setting('port')`.
- MySQL 5.6+: `@@server_uuid`. Also same when the source's replica status names the target: `SHOW REPLICA STATUS` (8.0.22+) `Source_UUID`, else `SHOW SLAVE STATUS` `Master_UUID`, equal to the target's `@@server_uuid`; and when both report the same non-empty `group_replication_group_name`. Before 5.6: `@@hostname:@@port` + start time (`UNIX_TIMESTAMP() − Uptime`, within 2 s) + `@@datadir`, all equal ⇒ same; hostname and port only ⇒ `Unsure`.
- SQLite: canonical parent directory + file name (the target file may not exist yet).

Reading from a replica and writing into its primary under the same database name is therefore refused: truncate or drop_recreate there would destroy the source's data through replication.

**Target facts read** (`TargetFacts.vars`): MySQL `version`, `version_comment`, `local_infile`, `max_allowed_packet`, `sql_mode` (global), `default_storage_engine` (`storage_engine` < 5.5.3), `character_set_server`, `collation_server`, `innodb_large_prefix`, `innodb_file_format`, `innodb_file_per_table`, `innodb_strict_mode`, `innodb_page_size`, `innodb_default_row_format`, `explicit_defaults_for_timestamp`, `lower_case_table_names`, `log_bin`, `log_bin_trust_function_creators`, `sql_require_primary_key`, `restrict_fk_on_non_standard_key`, `group_replication_transaction_size_limit`, `group_replication_group_name`, `wsrep_max_ws_size`, `wsrep_max_ws_rows`, `max_binlog_cache_size`, `read_only`, `server_uuid`, `hostname`, `port`, `datadir`, `Uptime`. PG `server_version_num`, `server_encoding`, `datcollate`, `datctype`, `datlocprovider`, `lc_monetary`, `is_superuser`, `system_identifier`, `pg_is_in_recovery`. SQLite `sqlite_version`, `free_bytes`. Missing variables are absent, never errors.

**MySQL privileges** from `SHOW GRANTS` (best effort): `CREATE`, `INSERT`, `DROP` (drop_recreate), `ALTER`, `INDEX`, `REFERENCES`, `CREATE VIEW`, `TRIGGER` on the database or `*.*`; missing → `TGT_PRIVILEGE`. An empty or failing `SHOW GRANTS` means unknown: no issue.

### 5.4 Cancellation and cleanup

`job_cancel` fires the token. Every target statement (load, INSERT, COUNT, probes, DDL, ANALYZE, REFRESH, setval) is raced against the token, and 300 ms after the cancel the writer calls `TargetSession::cancel_running` (PG cancel request; MySQL `KILL QUERY` from a side connection; SQLite `interrupt`). mysql_async polls the infile stream only between writes, so under server backpressure the stream's error would never be polled; the race, not the stream error, bounds the cancel. Source streams cancel their server query (existing driver cancel); PG sinks are dropped and their transaction rolls back; MySQL InnoDB chunks roll back; SQLite rolls back. The job reaches `cancelled` within `CANCEL_GRACE` (2 s). The server may still be rolling back a large InnoDB chunk after that: the result carries `SERVER_ROLLBACK_PENDING` (info) when a chunk was open.

Nothing is dropped automatically. `CopyResult.left_behind` lists what remains (the UI renders it in Korean): `created_table` (created by this run), `partial_rows` (rows committed so far: MySQL whole committed chunks, SQLite whole committed batches, as reported by `TargetSession::take_failed_outcome`, never the rows only sent to the writer; a MyISAM table may also keep part of the interrupted chunk), `emptied_table` (truncated, load not finished), `missing_post_data` (indexes, FKs, triggers not created), `sequence_not_set`.

### 5.5 Checkpoints, run claims and resume (`copy/checkpoint.rs`)

A separate rusqlite connection on `config.history_path()` (WAL, `busy_timeout 5000`):

```sql
CREATE TABLE IF NOT EXISTS copy_runs (run_id TEXT PRIMARY KEY, label TEXT NOT NULL, source_conn_id TEXT NOT NULL,
  target_conn_id TEXT NOT NULL, source_database TEXT NOT NULL, target_database TEXT NOT NULL,
  source_identity TEXT NOT NULL, target_identity TEXT NOT NULL,      -- ServerIdentity.key at the first start
  request_json TEXT NOT NULL /* CopyRequest without passwords and start-time fields */, plan_id TEXT NOT NULL,
  started_at TEXT NOT NULL, finished_at TEXT, state TEXT NOT NULL, last_job_id TEXT NOT NULL,
  owner_pid INTEGER NOT NULL, owner_instance TEXT NOT NULL, dry_run INTEGER NOT NULL DEFAULT 0, result_json TEXT);
CREATE TABLE IF NOT EXISTS copy_items (run_id TEXT NOT NULL, item_id TEXT NOT NULL, state TEXT NOT NULL,
  created_by_run INTEGER NOT NULL DEFAULT 0, rows_written INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL,
  error TEXT /* redacted */, PRIMARY KEY (run_id, item_id));
```

- **Write-ahead.** The intent is written before the action: `creating` with `created_by_run = 1` before CREATE; `loading` before TRUNCATE or the first chunk; `loaded`, `done`, `failed`, `skipped` after. `rows_written` = **committed rows** (whole committed MySQL chunks, PG table commit, SQLite batch commits; never the rows only sent to the writer). Dep-failed items (dependants of a failed item) get no checkpoint written. A checkpoint write failure emits `CHECKPOINT_FAILED` on the item, fails it, and marks the run non-resumable.
- **In use.** A run is in use when its state is pending, preflight, warned, running or finishing **and** either `owner_instance` is another live instance (`instances::is_alive`), or it is this instance and the job `last_job_id` exists in this process and is not terminal. So the desktop and the CLI can resume their own failed or cancelled runs in the same process. `copy_runs` reports a running run that is not in use as `failed` (interrupted).
- **Claim.** Start and resume claim inside `BEGIN IMMEDIATE`: a new run is inserted; a resume runs `UPDATE copy_runs SET owner_pid = ?, owner_instance = ?, last_job_id = ?, state = 'pending' WHERE run_id = ? AND <not in use>` and requires `changes() == 1` (else `RUN_IN_USE`). In the same transaction, another in-use run with the same `target_identity` + `target_database` gives `TARGET_IN_USE`.
- `copy_forget` refuses an in-use run. Runs are kept for 30 days or the newest 50, whichever keeps more; pruning skips unfinished runs. Copy runs do not go into the export history table.

**Resume** (`copy_resume(CopyResumeRequest)`): load `request_json`; take passwords from the request or the session store; add `confirm_production`; re-read both identities: a different target identity or database is `RESUME_TARGET_CHANGED_SERVER` (blocker: the connection was edited to point elsewhere since the run), a different source identity `RESUME_SOURCE_CHANGED_SERVER` (warning). Build a `ResumeState`, plan again. `build_plan` maps:

| checkpoint | resumed action |
|---|---|
| `done` / `skipped` | `resume_done` (`RESUME_DONE`, info); if the target object is gone: create + load again (`RESUME_TARGET_CHANGED`, warning) |
| `creating`, table missing | create + load |
| `creating`, table exists, nothing built on it (no dependant got past `pending`/`skipped`) | drop_recreate: DROP the table first (PG: release serial sequences the table owns before DROP), then create + load; `DESTRUCTIVE_DROP` (danger) if rows exist |
| `creating`, table exists, something was built on it | truncate + load, no DDL; `DESTRUCTIVE_TRUNCATE` (danger) if rows exist |
| `created` | truncate + load, no DDL; `DESTRUCTIVE_TRUNCATE` (danger) if rows exist |
| `loading` / `failed` in data, `rows_written = 0`, `created_by_run`, nothing built on it | drop_recreate (same as `creating` + exists, nothing built on it above) |
| `loading` / `failed` in data, table `created_by_run` or policy truncate/drop_recreate, other cases | truncate + reload. First a bounded probe `SELECT COUNT(*) FROM (SELECT 1 FROM t LIMIT <rows_written + 1>) q`: more rows than this run wrote (an application wrote meanwhile) → `DESTRUCTIVE_TRUNCATE` (danger); otherwise `RESUME_RELOAD` (info) |
| `loading` / `failed` in data, `append` into a pre-existing table, `rows_written = 0` (PG: nothing committed) | plain append again (`RESUME_APPEND_EMPTY`, info) |
| `loading` / `failed` in data, `append` into a pre-existing table, rows committed | `RESUME_APPEND_PARTIAL` (blocker) unless `partial = truncate` (reload; `DESTRUCTIVE_TRUNCATE`, danger) or `skip` (left as is, reported) |
| `loaded` | its post-data items only; finalize children (SequenceValue, AutoIncrement) are re-derived from the current source values, not re-checkpointed |
| post-data items | run when missing in the target |

The new job has a new id in the same `run_id`, `plan.resume_of = runId`, and becomes the owner. Dry-run runs are never resumable. The result view and the CLI summary show the stored policy (`CopyResult.existing`). Unit tests in `plan.rs` cover the mapping; checkpoint tests cover the three claim cases (own process after a failure resumes; another live instance is refused; two concurrent claims → one wins).

### 5.6 Verification and outcome

**row_count** (default): `rows_read` = rows streamed; target `count_rows` (`SELECT COUNT(*)`) after the load (for `append` also before, comparing the difference). Equal → `match`; else `mismatch` and the table is `failed` (`VERIFY_MISMATCH`). The count is raced against the token like every target statement.

**Never "match" with altered values.** A table with `data_warnings > 0` or skipped rows is `failed` (`DATA_WARNINGS` / `ROWS_SKIPPED`) and its verify outcome is `lossy`, even when the counts are equal. A table with `unmappable_chars > 0` is `lossy` too and either failed (`on_unmappable: fail`) or done with the problem `UNMAPPABLE_CHARS`.

**checksum**: while writing, each value is hashed in its canonical **written** text form (NULL marker, normalised temporal text, UTF-8 text, hex of binaries) with `std::collections::hash_map::DefaultHasher` per row, combined by wrapping add (order-independent). The MySQL source checksum is computed in the writer as it encodes (no second encoder). After the load the target table is read back through `TableDataSpec.verify_sql` (target names, same column order and casts; MySQL BIT is read back as ``CAST(`c` AS UNSIGNED)``, the unsigned value the writer hashed, so BIT(64) values ≥ 2^63, which reach the writer as negative Int64, still match) on a second target session that uses the load session's `time_zone` and `SET NAMES` (PG: the 4.2 settings) and is hashed the same way. PG→PG hashes the raw CSV records (record boundaries found by a quote-aware scanner) on both sides. For `append`, the read-back covers only the newly written rows (the target is read after the load and the count before the load is subtracted from the read-back result). Values go to `checksum_source/target` as hex. A checksum compares what BulkDB wrote with what the target stored: it **cannot** see losses in decoding or normalisation (U+FFFD, `?`, zero-date and invalid-date replacements), which are counted separately; the UI and CLI help say so, and that it reads the target once more.

PostgreSQL before version 12 printed floating-point values with a different number of significant digits (the `extra_float_digits = 3` format). Raw CSV bytes of float columns from a PG < 12 source do not match the PG 12+ shortest-exact representation that the target produces. Tables with float columns on a PG < 12 source are verified by row count only when `verify: checksum` is selected; each such table gets `CHECKSUM_NOT_APPLICABLE` (info, per item). `verify: none` skips both checks.

**FK orphans** (MySQL): every FK added under `foreign_key_checks = 0` (new tables, FK_CYCLE groups) is validated by `SELECT 1 FROM child c LEFT JOIN parent p ON <c.cols = p.cols> WHERE <c.cols all NOT NULL> AND p.<first col> IS NULL LIMIT 1` per FK; an orphan gives the problem `FK_ORPHANS` on that FK item (the constraint stays; counts only, no values).

**Skipped tables.** `skip_existing` tables are not loaded and count in `CopyResult.skipped_existing`. With `verify_skipped`, both sides get an exact `COUNT(*)`; a difference, or a plan-time `SKIPPED_DIFFERS`, is the problem `SKIPPED_DIFFERS` for that table.

**Outcome.**
- **failed**: any item failed (including `DATA_WARNINGS`, `ROWS_SKIPPED`, `VERIFY_MISMATCH`, `UNMAPPABLE_CHARS` under `fail`).
- **done with problems**: no failed item, but `CopyResult.problems > 0` (tables or items with `problems`: `UNMAPPABLE_CHARS` under `replace`, `INVALID_DATE`, `FK_ORPHANS`, `SKIPPED_DIFFERS`). The job state is `done`; the result view shows a danger banner and the CLI exits 5.
- **done**: everything else. `verified` = `verify ≠ none`, every loaded table `match`, and `problems == 0`.
- The result strip and the CLI summary say "N개 테이블은 이미 있어 데이터를 복사하지 않았습니다" when `skipped_existing > 0`.

## 6. Facade, IPC, CLI

### 6.1 `BulkDb` methods (thin delegations in `service.rs` to `copy::api`)

```rust
pub async fn copy_plan(&self, req: &CopyRequest, plan_token: Option<String>) -> Result<CopyPlan>; // no job; sessions `bulkdb-copyplan`
pub fn copy_plan_cancel(&self, plan_token: &str) -> Result<()>;                                  // CANCELLED within ~2 s
pub async fn copy_start(&self, req: CopyRequest, plan_id: String, acknowledged: Vec<String> /*ack keys*/) -> Result<String>; // job id
pub async fn copy_resume(&self, req: CopyResumeRequest) -> Result<String>;                       // job id
pub fn copy_status(&self, job_id: &str) -> Result<CopyStatus>;
pub fn copy_runs(&self, conn_id: Option<&str>, limit: usize) -> Result<Vec<CopyRunInfo>>;       // conn = source or target
pub fn copy_run(&self, run_id: &str) -> Result<CopyRunDetail>;
pub fn copy_forget(&self, run_id: &str) -> Result<()>;                                          // refuses an in-use run
pub fn copy_progress_snapshot(&self) -> Vec<CopyProgress>;                                      // full progress per active copy job
pub fn config_poll(&self) -> Result<()>;                                                        // section 1.2 (body: windows)
pub fn instances_info(&self) -> Result<InstancesInfo>;                                          // (body: windows)
```

`job_cancel`, `job_resume` (ack keys), `job_list`, `job_get` work for copy jobs. `remember_secret` (now `pub(crate)`) applies to both endpoints. Process state lives in `BulkDb.copy: copy::api::CopyState` (plan-token registry of `CancellationToken`s, active jobs by target for the lease registry, recent plans for `PLAN_CHANGED`, progress snapshots).

### 6.2 Tauri commands (args camelCase)

| command | args | returns |
|---|---|---|
| `copy_plan` | `req: CopyRequest, planToken?` | `CopyPlan` |
| `copy_plan_cancel` | `planToken` | null |
| `copy_start` | `req, planId, acknowledgedWarnings: string[]` (ack keys) | jobId |
| `copy_resume` | `req: CopyResumeRequest` | jobId |
| `copy_status` | `jobId` | `CopyStatus` |
| `copy_runs` | `connId?, limit?` (default 50) | `CopyRunInfo[]` |
| `copy_run` | `runId` | `CopyRunDetail` |
| `copy_forget` | `runId` | null |
| `window_init` | — (caller's `WebviewWindow`) | `WindowInit` |
| `window_new` | `connId?` | label |
| `window_list` | — | `WindowInfo[]` |
| `window_focus` | `label` | null |
| `window_set_connection` | `connId?` (caller's window) | null |
| `window_claim_job` | `jobId` (caller's window) | null |
| `instances_info` | — | `InstancesInfo` |
| `instance_spawn` | — | null |
| `app_quit_confirmed` | — | null |

**Events.** New broadcast: `copy:progress` (`CopyProgress` delta, `full: false`), `windows:changed` (`WindowInfo[]`), `connections:changed` (null), `settings:changed` (`Settings`), `prompt:resolved` (`{ promptId }`). New window-targeted (1.1): `jobs:claimed` (`{ jobIds }`), `app:quit-requested` (`{ jobs: number }`). The shell's copy loop polls `copy_progress_snapshot()` every 500 ms and emits, per job, the table entries changed since its last emission (≤ 64, active first) whenever anything changed and at least once a second (pure `copy_progress_delta(prev, next)`, unit-tested). `seq` grows with every change of a job's progress and of each table entry; the UI ignores entries whose `seq` is not newer than what it holds, and re-fetches `copy_status` on every `job:state` of a copy job, so a late full status never overwrites newer values and a lost delta is repaired. Unchanged: `job:state`, `job:done`, `job:progress`. Data rows never cross IPC; the plan carries DDL text and metadata only.

**TS wrappers** (`lib/commands.ts`): `copyPlan`, `copyPlanCancel`, `copyStart`, `copyResume`, `copyStatus`, `copyRuns`, `copyRun`, `copyForget`, `windowInit`, `windowNew`, `windowList`, `windowFocus`, `windowSetConnection`, `windowClaimJob`, `instancesInfo`, `instanceSpawn`, `appQuitConfirmed`. `lib/events.ts` adds the broadcast events; `lib/ipc.ts` adds `listenWindow` for the window-targeted ones.

### 6.3 CLI `bulk-cli copy`

```
bulk-cli copy plan   --source <CONN> --source-db <DB> [--source-schema S] --target <CONN> --target-db <DB> [--target-schema S] [opts] [--sql]
bulk-cli copy run    (same) [--expect-plan <PLAN_ID>] [--ack <CODE>]... [--yes] [--progress] [--dry-run] [--confirm-production <TARGET NAME>]
bulk-cli copy resume --run <RUN_ID> [--partial fail|truncate|skip] [--ack <CODE>]... [--yes] [--progress] [--confirm-production <NAME>]
bulk-cli copy runs   [--conn <CONN>] [--limit N]
bulk-cli copy status --run <RUN_ID>
bulk-cli copy forget --run <RUN_ID>
bulk-cli conn add|edit … --write-allowed true|false [--yes]     (conn list / conn show print write_allowed)
```

Options start from `CopyOptions::default()`; `--help` shows each default. `--tables a,b` / `--tables-file F` (one `schema.table` or `table` per line); `--exclude <ITEM_ID>` and `--exclude-table [s.]t` (repeatable); `--existing skip|truncate|drop-recreate|append` (default **skip**) and `--existing-for [s.]t=<policy>` (repeatable); `--content both|schema|data`; MySQL `--charset`, `--collation <name>|legacy`, `--table-charset t=cs`, `--table-collation t=coll`, `--column-as t.c=binary|text`, `--engine auto|keep|innodb|myisam`, `--zero-dates auto|null|keep|min`, `--time-zone +09:00` (fallback only), `--no-fix-row-size`, `--keep-definer`; PG `--pg-collation-fallback`, `--pg-source-encoding euc-kr|latin1|utf8`; names `--rename-schema a=b`, `--rename-table [s.]a=b`, `--lowercase-names`; loading `--indexes-with-tables`, `--concurrency 1..4`, `--batch-rows N`, `--batch-mb N`, `--target-lock-timeout SECONDS`; `--verify none|count|checksum`, `--verify-skipped`, `--on-unmappable replace|fail`; objects `--no-views --no-routines --no-triggers --no-sequences --no-types --no-extensions --no-comments --owners --grants --no-analyze --no-refresh`; source session `--work-mem`, `--lock-timeout`, `--fetch-batch`; passwords `--source-password-env VAR`, `--target-password-env VAR`. Saved connections only, by name or id. A CLI test asserts that a plan without option flags yields only `skip_existing` for existing tables.

Behaviour:
- `plan` prints the plan_id, the endpoints, the facts line (with the MySQL 6.x/7.x caption), then order, kind, name → target, action (with "exists · rows" / "exists · empty"), est rows, issue codes, and the acknowledgements needed (code, item count). `--sql` also prints every `target_sql` and the `session_sql` of both sides.
- `run` plans and prints like `plan`, then asks on a tty for each missing acknowledgement, listing the affected items; `--ack CODE` acknowledges every key of that code in this plan; `--yes` acknowledges all; without a tty and with acknowledgements missing it exits 4. `--expect-plan PLAN_ID` binds the run to a reviewed plan: a different plan_id prints the PLAN_CHANGED diff and exits 3. `--yes` never replaces `--confirm-production`.
- `--dry-run`: on PG the pre/post-data DDL is executed and rolled back; on MySQL and SQLite only the source SELECTs and the type mapping are checked; dry runs are never resumable.
- `--progress` prints one stderr line per second: phase, tables done/total, rows, MB/s, ETA, active table, RSS. Ctrl+C cancels the job and waits.
- The final summary states the outcome, the problems, the skipped-existing sentence and, on resume, the stored policy.
- `conn edit --write-allowed true` on a production-tagged connection asks on a tty, or requires `--yes`; without either it exits 4.
- `copy status --job` does not exist: every invocation is its own process with its own job registry; `--run` reads `history.sqlite`.

Exit codes (added to the help's exit-code list): **0** done (no problems; verified when verify ≠ none); **1** failed before a `CopyResult` existed (config, connect, internal error); **2** cancelled; **3** rejected (blockers) or `--expect-plan` mismatch; **4** acknowledgement or confirmation needed; **5** the job failed or ended with problems, with a stored, resumable `CopyResult`. The global `--json` prints exactly one JSON document to stdout: `CopyPlan` (plan), `CopyResult` (run / resume, at the end), `CopyRunInfo[]` (runs), `CopyRunDetail` (status).

## 7. UI (TablePlus-like, consistent with apps/desktop/DESIGN.md)

**Entry points:** Welcome left pane ghost row "데이터베이스 복사…" (`Copy` icon) under DBeaver import; Welcome row menu "이 연결을 원본으로 복사…" (source preset) / "이 연결을 대상으로 복사…" (target preset); palette action `copy.open` (in a workspace it presets the source to the workspace connection and the current browse database/schema); the workspace toolbar's connection-switcher menu (1.1); macOS File menu. No new toolbar button (DESIGN.md §5.C stays).

**CopyWizard** (`components/copy/CopyWizard.tsx`): `Sheet width="xl"` (880px), height `min(720px, 100% − var(--sheet-top) − 16px)`, mounted once in AppShell, open while `copy.wizard.open`.
- Header "데이터베이스 복사"; description "{source name} ({driver} · {host:port}/{db}[.{schema}]) → {target …}" with EnvTag chips (SQLite: the file path). The endpoint text comes from the Connection (never a secret) and, after planning, from `CopyServerFacts.endpoint`.
- Steps in a `SegmentedControl`: 1 원본·대상, 2 개체, 3 옵션, 4 계획; then the 진행 and 결과 views. Steps 2-4 are reachable only once both endpoints are valid.
- Footer: left "최근 복사…" (popover of `copy_runs` rows: 이어서 실행 / 결과 보기 / 기록에서 삭제); right [취소] [이전] [다음] / [계획 만들기]; on Step 4 [모의 실행] (secondary) and [복사 시작] (primary). Step 1 also offers [계획 만들기] directly when the selection is 전체 and the options are the defaults. [복사 시작] and [모의 실행] are disabled, with a tooltip naming the reason, until there are no blockers, every ack key is acknowledged and the production name matches.
- The sheet closes freely once a job runs; Activity's copy block [자세히] reopens it on `copy.showJob(jobId)`.
- `copy.open(preset)` always starts from `DEFAULT_COPY_OPTIONS`: destructive policies, overrides, `data_only` and dry runs are never remembered between opens. The target database starts empty unless explicitly preset.

**Step 1 원본·대상.** Two columns, each: connection `Select` (driver badge, name, EnvTag) with the endpoint caption under it; database `Select` from `catalog_databases` (MySQL target: plus "새 데이터베이스 이름…", created by the copy); PG schema `Select` with "모든 스키마" (target schema only when a source schema is chosen).
- When a chosen connection asks for its password every time and core has no session password for it, DbPasswordDialog opens for that side before its databases are listed; the password goes to core's session store (`remember_secret`) and never into wizard state. Resume uses the session passwords and asks only when one is missing.
- Only connections of the source's family are listed as targets.
- A connection without `writeAllowed` is disabled with "쓰기 허용 아님" and a [쓰기 허용으로 표시…] button → `askConfirm` (danger) "'{name}'에 쓰기를 허용할까요?", body "DB 복사가 이 연결에 테이블을 만들고 데이터를 넣거나 지울 수 있게 됩니다. 추출과 탐색은 계속 읽기만 합니다. 서버를 바꾸면 해제됩니다." (+ "운영(production) 연결입니다." when tagged). Accept → `markWriteAllowed(connId)` (`conn_save` of the stored connection with `writeAllowed: true`).
- Same connection and same database: inline danger text "원본과 같은 데이터베이스에는 복사할 수 없습니다", 다음 disabled. Core's `SAME_DATABASE` is the authority for other connections pointing at the same server; a test asserts both agree for the same connection.
- A non-production source with a production-tagged target shows a danger `Notice` "운영 데이터베이스에 씁니다".

**Step 2 개체.** 전체 / 테이블 선택 `SegmentedControl`; tree schema → tables (28px rows: checkbox, name, `RowEstimate variant="compact" showLabel`, size); search; 모두 선택 / 모두 해제. PG adds a "기타 개체" group of switches bound to `CopyObjectFlags`, with counts when known: 뷰, 함수·프로시저, 트리거, 시퀀스 값 ("끄면 대상의 시퀀스·AUTO_INCREMENT 값을 맞추지 않습니다"), 타입, 확장, 주석, 소유자, 권한.

**Step 3 옵션.** Visible:
- 이미 있는 테이블: **건너뛰기 (기본)** / 비우고 적재 / 삭제 후 다시 만들기 / 이어 붙이기 (the two destructive choices in danger text).
- 내용: 구조와 데이터 / 구조만 / 데이터만.
- 검증: 안 함 / 행 수 (기본) / 체크섬 ("대상을 한 번 더 읽습니다. 문자 변환 손실은 체크섬으로 알 수 없으며 따로 집계됩니다").
- A facts line summarising the MySQL defaults, e.g. "utf8mb4 · utf8mb4_0900_ai_ci · InnoDB · 0 날짜: NULL로".

Under a collapsed **고급** disclosure: MySQL 문자 집합, 정렬 규칙 (with "기존 정렬과 호환 (PAD SPACE)"), 엔진, 0 날짜 (자동 / NULL로 / 유지 / 최솟값으로, one-line explanation each), 정의자 유지, 변환할 수 없는 문자 (바꾸고 계속 / 실패); PG 정렬 대체, 원본 인코딩 (SQL_ASCII sources only); 대상 이름 소문자로; editable rename list (스키마/테이블); 인덱스·제약은 데이터 적재 후 생성; 동시 테이블 수 1-4 (help "원본 세션 {n}개 · 대상 세션 {m}개"); 배치 행 수; 대상 잠금 대기 (초, 기본 5); 건너뛴 테이블도 행 수 비교.

**Step 4 계획.**
- [계획 만들기] calls `copy_plan` with a fresh plan token; "계획 계산 중…" with [취소] (`copy_plan_cancel`). Any draft change after a plan exists clears it (the step shows [계획 만들기] again).
- Facts line: both endpoints, then e.g. "MySQL 8.0.36 · utf8mb4 / utf8mb4_0900_ai_ci · InnoDB · LOAD DATA 꺼짐 → INSERT 배치 · 청크 64 MiB · 스냅샷 없음 · 원본 세션 2 · 대상 세션 3"; for MySQL the caption "MySQL 6.x·7.x는 정식 출시되지 않았습니다 (6.0은 취소된 알파, 7.x는 NDB Cluster 번호)", and a Notice when a version contains `-ndb-`. Counts strip by action (destructive counts in danger), plus "이미 있음 {n} (빈 테이블 {m})".
- 차단 rows (danger; item blockers with [제외], which adds the item id to `options.exclude` and re-plans) and 경고 rows from `plan.acks`, one per code with its affected item count and a "확인했습니다" checkbox bound to the key, as in PreflightSheet. `ENUM_DUPLICATE`, `COLLATION_UNIQUE_RISK` and `UNIQUE_PREFIX` rows offer "이 테이블만 정렬 규칙 바꾸기…" (→ `mysql.table_overrides`, re-plan).
- Item table (virtualized above 200 rows, 28px): 순서, kind icon + `name → target_name`, 작업 chip (생성 / 건너뜀(있음) / 비우고 적재 / 삭제 후 생성 / 이어 붙이기 / 실행 / 지원 안 함 / 제외됨 / 완료됨(이어서)), target state (있음 · 데이터 있음 / 있음 · 비어 있음), 예상 행, 경고 count. For existing tables the 작업 chip is a menu (건너뛰기 / 비우고 적재 / 삭제 후 다시 만들기 / 이어 붙이기) writing `existing_overrides` and re-planning. Rows listed by `PLAN_CHANGED` are highlighted. Selecting a row opens a split below: tabs 실행할 SQL | 원본 DDL (mono `<pre>`, copy button) and that item's issues.
- A "세션 설정" section lists `session_sql.source` and `session_sql.target`.
- Production target: danger `Notice` with the target endpoint and an input "확인을 위해 대상 연결 이름을 입력하세요: {name}". The typed name lives in wizard state (`confirmName`), outside `draft`.
- [복사 시작] → `start()`; [모의 실행] → `start({ dryRun: true })`. Both merge the start-time fields, call `copy_start(req, plan.planId, ackKeys)` and `jobs.markLocal(jobId)`.

**Progress view.** Overall `ProgressBar`; `{rows}행 · {size} · {MB/s} · 경과 · 남은 시간`; phase (계획 / 구조 생성 / 데이터 적재 / 인덱스·제약 / 마무리 / 검증); filter 전체 / 진행 중 / 실패 / 완료; table rows (28px): state chip (대기 / 생성 중 / 적재 중 / 인덱스 생성 중 / 검증 중 / 완료 / 건너뜀 / 실패 / 취소됨 / 계획됨), name, `rows_written / est_rows` + %, rate, elapsed, method chip (COPY / LOAD DATA / INSERT), error in a tooltip. [중지] → `askCancel(jobId)`. Initial state from `copy_status` (an absent plan shows "계획 계산 중…"); `copy:progress` deltas merge by `item_id` and `seq` (6.2).

**Result view.**
- Summary strip: 완료 / 실패 / 건너뜀, duration, 검증 통과 or "검증 불일치 {n}건" in danger; "문제가 있는 테이블 {n}개" danger banner for done with problems; "N개 테이블은 이미 있어 데이터를 복사하지 않았습니다"; on resume "이미 있는 테이블: {policy}".
- Dry run header per family: PG "구조 DDL을 실행해 본 뒤 모두 되돌렸습니다"; MySQL/SQLite "원본 SELECT와 형식 변환만 확인했습니다 (대상에는 아무것도 실행하지 않았습니다)"; button [실제로 복사].
- Verification table (테이블, 읽은 행, 대상 행, 결과 ✓ / ✗ / 값 변경됨 (lossy), 경고 with `warning_samples` tooltip); value fixes per column (e.g. "created_at: 0 날짜 → NULL 12건, 잘못된 날짜 3건"); runtime warnings (`UNMAPPABLE_CHARS` "변환할 수 없는 문자 {n}개", `FK_ORPHANS`, `SERVER_ROLLBACK_PENDING`); the checksum caveat.
- `left_behind` Notice "대상에 남은 것" (만든 테이블 / {rows}행까지 적재됨 / 비워진 테이블 / 만들지 못한 인덱스·제약·트리거 / 설정되지 않은 시퀀스).
- Buttons: [이어서 실행] (when `resumable`; asks for `partial` on `RESUME_APPEND_PARTIAL` and for the production name when tagged), [쓰기 허용 해제] (target, `conn_save` with `writeAllowed: false`), [계획 다시 보기], [닫기].

**Activity.** For kind `copy`, `JobRow` uses the `Copy` icon and label "복사: {src}.{db} → {tgt}.{db}" and renders the slot `components/copy/CopyJobSummary.tsx` (tables done/total, failed count, [자세히]). For a **warned** copy job, `JobRow`'s [검토] and `reviewWarnings` call `useCopyStore.getState().showJob(jobId)` instead of the export PreflightSheet: the wizard opens on the plan step with `copy_status.plan`, the new issues, their acknowledgement checkboxes and the `PLAN_CHANGED` highlights, and its primary button calls `jobs.resume(jobId, ackKeys)`.

**Strings.** `components/copy/strings.ts` = `defineStrings('copy', { en, ko })`; every visible string has an English fallback; issue titles under `issue.<CODE>`. Codes that need Korean entries added: `UNKNOWN_ITEM`, `ENUM_ERROR_VALUE`, `ENUM_ERROR_NULL`, `CHECKSUM_NOT_APPLICABLE`, `DRY_RUN_NOT_CHECKED`, `SEQUENCE_FLOOR_UNREADABLE`, `CHECKPOINT_FAILED`, `COLLATION_UNSUPPORTED`, `RESUME_APPEND_EMPTY`. Glossary: 데이터베이스 복사 Copy database · 원본 Source · 대상 Target · 쓰기 허용 Write allowed · 개체 Objects · 계획 Plan · 모의 실행 Dry run · 실제로 복사 Copy for real · 건너뛰기 Skip · 비우고 적재 Truncate and load · 삭제 후 다시 만들기 Drop and recreate · 이어 붙이기 Append · 제외 Exclude · 이어서 실행 Resume · 검증 Verify · 고급 Advanced · 세션 설정 Session settings · 새 창 New window · 새 BulkDB 실행 New BulkDB instance · 창 Window.

**Stores.** `stores/copy.ts` (copy-ui) imports `connections`, `jobs`, `catalog`, `ui` only:

```ts
export type CopyStep = 'endpoints' | 'objects' | 'options' | 'plan' | 'progress' | 'result';
export interface CopyWizardState { open: boolean; step: CopyStep; draft: CopyRequest /*dryRun false, no confirmProduction*/;
  plan: CopyPlan | null; planBusy: boolean; planToken: string | null; error: string | null;
  acks: string[] /*AckNeed keys; a new plan keeps only keys present in it*/; confirmName: string /*start-time*/; jobId: string | null }
export interface CopyState {
  wizard: CopyWizardState; runs: Record<string, CopyStatus>;              // by jobId
  open(preset?: { source?: Partial<CopyEndpoint>; target?: Partial<CopyEndpoint> }): void; close(): void;
  setDraft(patch: Partial<CopyRequest>): void /*clears the plan*/; setStep(s: CopyStep): void; toggleAck(key: string): void;
  setConfirmName(name: string): void; buildPlan(): Promise<void>; cancelPlan(): Promise<void>; exclude(itemId: string): Promise<void>;
  setItemPolicy(itemId: string, policy: ExistingTablePolicy | null): Promise<void>; start(opts?: { dryRun?: boolean }): Promise<string | null>;
  resume(runId: string, partial?: ResumePartial, confirmProduction?: string): Promise<string | null>;
  showJob(jobId: string): Promise<void>; applyProgress(p: CopyProgress): void;
  markWriteAllowed(connId: string): Promise<boolean>; clearWriteAllowed(connId: string): Promise<boolean>; }
export const DEFAULT_COPY_OPTIONS: CopyOptions;  // equals components/copy/copy-options-default.json (= serde defaults)
```

`buildPlan` keeps a plan only when the draft did not change since the call (generation counter). `CopyWizard` subscribes to `copy:progress` while mounted; `AppEvents` has no copy logic beyond the catalog invalidation of 1.1. Vitest cases include: acknowledge a code, change the policy, re-plan → [복사 시작] is disabled again; typing the production name does not invalidate the plan; `open()` resets a previous truncate policy.

`stores/windows.ts` (windows; the skeleton creates it with this interface and no-op bodies):

```ts
export interface WindowsState { self: WindowInit | null; windows: WindowInfo[]; instances: InstancesInfo | null;
  apply(ws: WindowInfo[]): void; openNew(connId?: string): Promise<void>; focus(label: string): Promise<void>;
  othersWith(connId: string): WindowInfo[]; refreshInstances(): Promise<void>; spawnInstance(): Promise<void> }
export function initWindows(): Promise<void>;
```

## 8. Safety model

1. **Write permission.** Only a connection with `writeAllowed: true` can be a target (`TARGET_NOT_WRITABLE`). It is set explicitly, with a warning, in the wizard or by `conn edit --write-allowed true` (production: tty confirmation or `--yes`); never on a new or duplicated connection; cleared when the connection is pointed at another server. The result view offers to clear it. Every other feature stays read-only on every connection.
2. **Production.** A `production` target requires typing its name (`confirm_production`), checked by core at `copy_start` and `copy_resume` (never stored in the run); `--yes` never replaces it.
3. **The source is never written.** It is used only through `Source::stream` with BulkDB SELECTs that pass sqlguard, and through catalog reads on read-only sessions (data, catalog and plan sessions alike; PG also `row_security = off`); SQLite sources open read-only. `SAME_DATABASE` refuses a target that is the source database on the same server, including a primary behind the replica being read, and `SOURCE_IS_REPLICA` / `SAME_SERVER_UNSURE` require an acknowledgement when that cannot be ruled out. Copied MySQL views are re-qualified to the target database; triggers that name the source database are flagged.
4. **Generated SQL only.** No user DML/DDL. Every target statement is generated by BulkDB or taken from the source's catalog definition functions (`SHOW CREATE`, `pg_get_*def`), checked to be one statement with an allowed leading keyword for its family (5.2); dry runs have their own narrower list. Identifiers are always quoted. Values never enter SQL text except as escaped literals on the MySQL INSERT path (`quote_str` / hex under BulkDB's own session `sql_mode`, UTF-8 connection) and numeric setval / AUTO_INCREMENT arguments; SQLite binds parameters. The LOAD DATA handler serves only BulkDB's token, never a disk file.
5. **Review before writing.** The plan shows every statement per item, the load templates, the truncate groups, `CREATE DATABASE` and the session settings of both sides. Destructive actions, skipped existing tables, replicas, missing snapshots and the other warnings need an acknowledgement keyed by code and affected items; the default policy is **skip**; a dry run executes nothing on MySQL and SQLite and only rolled-back CREATE/ALTER/COMMENT/GRANT DDL on PG. `plan_id` binds the start to the reviewed plan.
6. **Server protection.** The source is read through the existing streaming drivers with the export session settings and preflight checks; at most 4 tables at once (default 1), with the session totals shown; MyISAM lock, snapshot and long-snapshot warnings. On the target: bounded lock waits (`target_lock_timeout_s`), chunks below replication transaction limits, one writer per target database (lease), drops with RESTRICT. Cancel in about 2 s; global server variables are never changed.
7. **Secrets** stay in the keyring and session memory; `request_json` has no passwords; logs are masked.
8. **Data stays in core.** No data preview. The charset sample of 3.2 is the only read of values outside the copy stream; its values never leave the planner. Server messages are redacted (`redact_values`) before they reach IPC, history or logs.

## 9. Testing

**Unit tests, next to the code.**
- copy-schema: SHOW CREATE TABLE parser on fixtures from 4.0 (`TYPE=MyISAM`, `timestamp(14)`, `timestamp(12)`, `timestamp(8)`, `timestamp(6)`, `varchar(20) binary`, Korean ENUM/COMMENT), 4.1, 5.5, 5.7, 8.0 (CHECK, `/*!50100 PARTITION BY …*/`, functional and DESC key parts, invisible index, expression default, STATS_* options) and an unknown clause → `UNPARSED_CLAUSE`; golden target DDL for 5.0, 5.5.2, 5.6 COMPACT, 5.7, 8.0, 8.4, 9.0; collation mapping incl. PAD notes and the `legacy` preset; TEXT promotion and byte-sized TEXT/MEDIUMTEXT replacement; InnoDB in-row size per row format; DECIMAL widening; the 4.0 first-TIMESTAMP rule; per-column reads by version (CAST vs CONCAT, UNIX_TIMESTAMP below 4.1.3, `c + 0` YEAR, `c + 0e0` FLOAT); zero-date rewriting per policy incl. PK/UNIQUE keys; index prefixes and `UNIQUE_PREFIX`; `NAME_COLLISION` with `lower_case_table_names=1`; 8.4 `FK_NON_STANDARD_KEY`; engine rules, `AUTOINC_GROUPED`, `FK_ON_MYISAM`; DDL placement (one ALTER per table, DISABLE/ENABLE KEYS, < 5.6 single ALTER); view qualifier rewriting with different database names (golden) and `DB_QUALIFIED_REFS`; view dependency order; trigger statements from SHOW TRIGGERS on 5.0; charset sampler classification (latin1 holding CP949, euckr holding UTF-8, binary attribute, BLOB text); PG builders over canned catalog rows (identity with SEQUENCE NAME vs serial, partitions and skipped partition children, enum/domain incl. NOT VALID domain checks, NOT VALID table checks in post_data, extension members skipped, extconfig data items, inheritance with local columns/constraints, collations, column attributes, replica identity, invalid indexes skipped, internal triggers skipped, trigger enable states, view reloptions incl. `security_invoker`, PG18 `contype 'n'` and NOT ENFORCED, function header rewrite on rename, topological order, FK/trigger placement, setval templates and the never-decrease rule, `PG17_SEARCH_PATH`, `SRC_RLS`, `PG_SQL_ASCII_SOURCE`, `PG_LARGE_OBJECTS`, `PG_DEFAULT_COLLATION_DIFFERS`); SQLite planning (escaped LIKE, virtual and shadow tables); action derivation for 4 policies × exists/missing × overrides, row-presence probes, drop dependants (`DROP_HAS_DEPENDANTS`, drop order), `tables` selection, `exclude`, truncate FK groups as `TruncateGroup` items, `TABLES_SKIPPED_EXISTING`, `SKIPPED_DIFFERS`, `SEQUENCES_NOT_SET`, resume mapping incl. `creating`; item id format.
- copy-data: `mysql_encode` (LOAD DATA field encoding of each escaped byte, NULL vs empty, hex binary, BIT, Korean UTF-8; INSERT literals and packet splitting; pieces end on row boundaries); `normalize_temporal` (epoch → UTC, compact widths for the fallback, zero, partial and invalid dates '2004-02-31' / '2004-04-31' × policies, YEAR 0, counting); `timestamp_min_literal` for '+00:00', '+09:00', '−05:00'; the session prelude and sql_mode builder per policy and version; the session assertion; `TokenHandler` refusing other names; LOAD/INSERT warnings failing the table; chunk sizing from replication limits; `ROW_TOO_LARGE` before sending; checkpoint CRUD with two connections and busy_timeout; write-ahead states; run claims (own process, other live instance, concurrent claims) and `copy_forget` refusal; identity / `same_server` for every rule (system_identifier, replica UUID, group name, < 5.6 hostname/start time/datadir, SQLite paths); lease names; verification outcomes incl. `lossy` and problems; checksum stability; the semaphore budget (`Budgeted` permits released after encoding); a blocked writer reaching `cancelled` within 2 s; statement allow-lists per family and the dry-run list (incl. rejected `SET GLOBAL`, `DROP SCHEMA`, `DELETE`); `redact_values` on real samples; `plan_id` stability (plan time vs start time with `confirm_production`; env issues not hashed); ack keys change with the affected item set; `CopyOptions` defaults equal `copy-options-default.json`.
- windows: config lock read-merge-write with two `ConfigStore`s on one dir (no lost update) and the `write_allowed` rules inside `upsert`; `reload_if_changed` with changed ids; per-pid log names and pruning; instance files (lock before json, stale removal, `is_alive` by instance id, own pid skipped); `platform::instance_command`; registry index/title/prompt routing and job hand-over; `copy_progress_delta` with `seq`; capability windows list; menu routes to the focused window; `PromptResolved` on timeout.
- UI (vitest): copy store (generation guard, ack keys across re-plans, plan cleared by draft changes but not by the production name, production gate, exclude, item policy menu, progress merge by `seq`, `open()` resets options, dry run via `start({dryRun})`), wizard steps and gating, target disabled until write-allowed, same connection + database refusal, warned copy job opening the plan step, `listenWindow` target option, `duplicateConnection` without `writeAllowed`, welcome window chips, windows store, `Mod+Shift+N` keymap row, mock-copy and mock-windows flows.

**Local end-to-end without docker** (`crates/core/tests/copy_tests.rs`, copy-data):
- **SQLite → SQLite:** default **skip** with one table already in the target (skipped, untouched, `TABLES_SKIPPED_EXISTING` needs its acknowledgement); run + count verification; truncate, drop_recreate, append; `existing_overrides`; cancel and resume, made deterministic: `batch_rows = 1000`, poll `copy_status` until `rows_written > 0`, then hold the target's write lock from a second rusqlite connection (`BEGIN IMMEDIATE`) so the writer blocks, cancel, assert `cancelled` within 3 s, release the lock, `copy_resume` reloads only the interrupted table and ends `done`; dry run leaves the target file byte-identical and creates no missing target file; refusal without `writeAllowed`; production confirmation refusal and success; `SAME_DATABASE`; source file unchanged (hash before/after); `TARGET_IN_USE` with two `BulkDb` instances on one config dir; **RSS**: wide rows, concurrency 4, peak RSS < 100 MB via `memory-stats`; `bulk-cli copy` exit codes 0/3/4/5 and `--expect-plan` in a temp config dir.
- **Fake MySQL 4.0 EUC-KR → `RecordingTarget`** through `BulkDbOptions.copy_target_opener` (a saved mysql connection with `writeAllowed`; facts MySQL 8.0.36, `local_infile` OFF and ON): the fake (`Persona::Mysql40 { charset_name: "euc_kr", charset_id: 19, .. }`) answers the 3.2 SQL with CP949 bytes for a table with `id int(11) auto_increment`, `name varchar(20)` (Korean), `kind enum('가','나')`, `created timestamp(14)`, `stamp8 timestamp(8)`, `stamp6 timestamp(6)`, `stamp12 timestamp(12)` (non-midnight times, answered as UNIX_TIMESTAMP epochs), a zero `date` and the rows '2004-02-31' and '2004-04-31', `price decimal(8,2)`, `ratio float`, a blob. Assert the recorded DDL (`utf8mb4`, `utf8mb4_0900_ai_ci`, `ENGINE=InnoDB`, `timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`, `decimal(9,2)`, enum values in UTF-8), that every target session (DDL and LOAD) carries the prelude (`SET NAMES utf8mb4`, the strict sql_mode, `time_zone '+00:00'`), the recorded LOAD payload / INSERT literals (valid UTF-8 with the expected Korean, `\N` for zero and invalid dates under `null`, the TIMESTAMP minimum '1970-01-01 00:00:01' for the first TIMESTAMP's zero values, full date-times for the narrow TIMESTAMPs, hex for the blob), the `value_fixes` counts, `INVALID_DATE` as a problem, and the counts. A second persona with `charset_name: "latin1"` serving CP949 bytes must give the `SRC_LATIN1_KOREAN` blocker.

**CI integration tests** (`crates/core/tests/copy_it.rs`, `--features it`, copy-it). `.github/workflows/ci.yml` and `docker/compose.yml`:

| service | port | env |
|---|---|---|
| existing `postgres:15` | 5432 | `BULKDB_PG_URL` |
| `postgres:16` (new) | 5433 | `BULKDB_PG16_URL` |
| `postgres:17` (new) | 5434 | `BULKDB_PG17_URL` |
| existing `mysql:8` | 3306 | `BULKDB_MYSQL_URL` |
| existing `mysql:5.5` euckr (step) | 3307 | `BULKDB_MYSQL55_URL` |
| `mysql:9` (new; root uses caching_sha2_password) | 3308 | `BULKDB_MYSQL9_URL` |
| optional MySQL 4.0 (an image built from the 4.0.27 binaries, when available) | 3309 | `BULKDB_MYSQL40_URL` |

- mysql 5.5 euckr → mysql 8 and → mysql 9: Korean text, ENUM, zero dates (`null`, `keep`), invalid dates, TIMESTAMP, YEAR, DECIMAL, BLOB, BIT; both load methods; a 5 MB blob against a 4 MB `max_allowed_packet` fails with `ROW_TOO_LARGE` (proving the UNHEX NULL behaviour is caught); a deliberately truncating value fails the table with `DATA_WARNINGS`; checks: `HEX()` of target bytes is UTF-8, `SHOW CREATE TABLE` has utf8mb4/InnoDB, counts match.
- MySQL 4.0 (when the env var is set): pins `UNIX_TIMESTAMP(c)` for `timestamp(8)`/`(6)`/`(12)`, `c + 0e0` for FLOAT and `c + 0` for YEAR(2) against real output.
- pg15 → pg16 and → pg17: enum, domain (incl. a NOT VALID check with violating legacy rows), identity, serial, a partitioned table, an inheritance parent and child, a view with `security_invoker`, a matview, a function, a trigger that must not fire during the load and a disabled trigger, a deferrable FK, a NOT VALID check over violating rows, comments, the `citext` extension, an RLS table read by a non-owner (`SRC_RLS`), a function used by an expression index without `search_path` (`PG17_SEARCH_PATH` on 17); checks: next sequence values, counts, checksum verification.
- pg15 → pg15 into a second database; cancel + resume on PG; a dry run with the truncate policy into an existing serial table leaves its `last_value` unchanged; an append into a table whose sequence is ahead leaves it ahead.
- Isolation: the MySQL tests toggle `SET GLOBAL local_infile` (root; test setup, not BulkDB behaviour) only inside one test per server, sequentially, guarded by a static `tokio::sync::Mutex` per server in `copy_it.rs`. Every test uses a unique target database or schema (`bulkdb_it_<uuid>`) and drops it at the end.
- Tests skip themselves when their env var is missing, like the existing it-tests.

## 10. File ownership and integration

### 10.1 Skeleton (W0, owner **shared**; done before the five owners start; compiles, all tests green, `pnpm typegen` run)

**Core:**
- `copy/mod.rs` declaring `types`, `api`, `schema`, `translate`, `plan`, `exec`, `data`, `target`, `checkpoint`, `verify`, `preflight`, `testkit`; `pub struct CopyJob` with `info() -> CopyJobInfo`.
- `copy/types.rs` with every type of section 2, exactly (including `Budgeted`, `LoadInput`, `TargetOpener`, `CheckpointState::Creating`, `CopyStatus.plan: Option`, the `seq` fields).
- Stubs returning `CoreError::Internal("copy: not implemented")` for `api::*`, `plan::build_plan`, `schema::source_meta`, `schema::read_finalize_values`, `schema::read_target_floor`, `translate::finalize_sql`, `exec::run_copy_job`; `testkit.rs` with working `RecordingTarget` and `CannedMeta` on the frozen API of section 2 (copy-schema's tests need them in W1).
- `BulkDb` gains `pub(crate) copy: copy::api::CopyState` (`Default`; owned by copy-data); `remember_secret` becomes `pub(crate)`; `BulkDbOptions.instance_kind` and `copy_target_opener`.
- `lib.rs`: `pub mod copy; pub mod instances;`. `instances.rs` stub (`register`, `info`, `is_alive`; windows fills it). `ConfigStore::reload_if_changed` stub returning no change. `platform::instance_command` / `spawn_instance` stubs (windows fills them).
- `types.rs` additions (section 2) and their `ts_export.rs` entries, including the prompt context fields.
- `job.rs`: `JobEntry.copy`, `create_copy(…, make)`, `summary().copy`.
- `service.rs`: every section 6.1 method as a delegation; `ConnectionsChanged` / `SettingsChanged` emits; `conn_save` passes `write_allowed` through to `ConfigStore::upsert` (whose rules windows implements).
- `prompts.rs`: `PromptResolved` on every removal (answer, timeout, forget, drop guard).
- `source/mod.rs`: `fn as_any(&self) -> &dyn std::any::Any` on `Source` (every driver); `pub fn mysql_raw(db: &dyn Database) -> Option<&dyn MysqlRawQuery>` (modern or legacy); `pub(crate) fn opts(&self) -> &mysql_async::Opts` on `MySqlDatabase`; `Query.max_batch_bytes` honoured by every reader; `SourceStream.replaced` filled by the MySQL readers.
- `preflight.rs`: `concurrent_session_issue` and the session listing become `pub(crate)`.
- CLI: `BulkDbOptions { instance_kind: Some("cli"), .. }`; the desktop state passes `"desktop"`.
- CLAUDE.md: one line under "Also": "DB copy (docs/db-copy.md) is the only writer: generated SQL, write-allowed targets only."

**Tauri:** `commands.rs` copy wrappers (about 10 lines each) and window/instance command stubs (`window_init` → `{label, index: 1}`, `window_list` → `[]`, the others no-ops), all registered; `events.rs` maps the three new `CoreEvent`s and runs the `copy:progress` loop.

**Frontend:**
- `lib/commands.ts` wrappers, `lib/events.ts` events and `lib/ipc.ts` `listenWindow` (6.2), with the per-window bus in mock mode.
- `lib/mock-ipc.ts` exports `MockCtx { emit, emitTo, log, findConn, nowIso, after, every, clear, jobs: { create(kind, connId, label, copy?), setState, finish, setProgress, onCancel } }`, dispatches `copy_*` to `mock-copy.ts` `createCopyHandlers(ctx)` and `window_*` / `instance*` / `app_quit_confirmed` to `mock-windows.ts` `createWindowHandlers(ctx)`; `reset()` resets both.
- Slot stubs with the header `// SLOT STUB (skeleton). Owner: <key>. Replace this file entirely.`: `mock-copy.ts`, `mock-windows.ts`, `stores/copy.ts` (`open` / `close` only), `stores/windows.ts` (7, no-op bodies), `components/copy/CopyWizard.tsx`, `components/copy/CopyJobSummary.tsx`; `components/copy/copy-options-default.json` with the serde defaults (copy-data's defaults test reads it in W1).
- AppShell mounts `<CopyWizard />` and calls `initWindows()`; `JobRow` renders `CopyJobSummary` for kind `copy` and the "다른 창에서 시작" caption; `JobRow` [검토] / `reviewWarnings` branch kind `copy` to `useCopyStore.getState().showJob(jobId)`; ActivityPanel footer note (1.2).
- `stores/jobs.ts`: `local` / `markLocal` (calling `window_claim_job`); export and count starts call it.
- AppEvents handles `connections:changed`, `settings:changed`, `windows:changed` (→ `useWindowsStore.getState().apply`), `prompt:resolved`, the catalog invalidation on copy `job:done`, and through `listenWindow` `menu:action`, `tunnel:hostkey`, `tunnel:password`, `jobs:claimed`, `app:quit-requested`.
- HostKeyDialog / SshPasswordDialog show the prompt context; ConnectionDialog shows the write-allowed row with [해제]; `ConnectionDialogForm.toInput` omits `writeAllowed`; `duplicateConnection` deletes it.
- `builtin-actions.ts` registers `copy.open` (→ `useCopyStore.getState().open(preset)`), `window.new`, `window.openConnection` and `app.newInstance` (→ windows store); `keymap.ts` row `mod+shift+n` → `window.new` (`menuOwned`).
- DESIGN.md §6 row and a §7.6 slot entry for `CopyWizard` / `CopyJobSummary`.

### 10.2 Owners (W1, in parallel, disjoint files)

| key | files |
|---|---|
| **windows** | `apps/desktop/src-tauri/**` (the entire shell, incl. the copy command wrappers the skeleton added: `windows.rs` registry, window commands, menu/prompt routing, job claims and hand-over, Close Window, quit confirmation, last-window shutdown, menu items `window.new` / `app.newInstance` / `copy.open`, capabilities, the 2 s `config_poll` loop); multi-process safety in `crates/core/src/config.rs` (incl. the `write_allowed` rules in `upsert`), `crates/core/src/logging.rs`, `crates/core/src/history.rs`, `crates/core/src/instances.rs`, `crates/core/src/platform/**` (instance spawning); the **bodies** of `BulkDb::config_poll` and `BulkDb::instances_info` in `service.rs` (method level); `apps/desktop/src/components/welcome/**` (incl. the copy entry points, PenLine icon, 쓰기 허용 해제), `apps/desktop/src/components/layout/**` (incl. the toolbar copy entry and the status pill's copy list), `apps/desktop/src/stores/windows.ts`, `apps/desktop/src/lib/mock-windows.ts` |
| **copy-schema** | `crates/core/src/copy/schema/**`, `crates/core/src/copy/translate/**`, `crates/core/src/copy/plan.rs` (schema reading per family and version, the charset sampler, DDL translation, target existence, row-presence and dependant checks of section 3, action derivation incl. selection, overrides, exclude and resume mapping, plan building incl. Database and TruncateGroup items, finalize value readers and `finalize_sql`; unit tests inside) |
| **copy-data** | `crates/core/src/copy/**` except `schema/`, `translate/`, `plan.rs` (types, api and `CopyState`, exec, data writers and `mysql_encode`, target sessions and the opener, leases, checkpoints/claims/resume, verification, preflight/identity/plan_id/acks, redaction, statement allow-lists, testkit), the copy methods in `crates/core/src/service.rs`, `crates/cli/**` (`bulk-cli copy`, `conn --write-allowed`, exit code 5), `crates/core/tests/copy_tests.rs` |
| **copy-ui** | `apps/desktop/src/components/copy/**` (incl. `copy-options-default.json` after W0, `strings.ts`), `apps/desktop/src/stores/copy.ts`, `apps/desktop/src/lib/mock-copy.ts` |
| **copy-it** | `crates/core/tests/copy_it.rs`, `.github/workflows/ci.yml`, `docker/compose.yml` |
| **shared** | everything else: `crates/core/src/types.rs`, `ts_export.rs`, `lib.rs`, other core modules (`source/**`, `job.rs`, `preflight.rs`, `prompts.rs`, `encoding.rs`, …), `apps/desktop/src/lib/**` (except the two mock files above), stores (except `copy.ts`, `windows.ts`), other components (`monitor/**`, `dialogs/**`, `palette/**`, …), `DESIGN.md`, `docs/**`, `CLAUDE.md`, `packages/ipc-types` (regenerated only by `pnpm typegen`) |

**Rules.**
- Owners never edit another key's files; a need goes into `notDone` for the shared owner.
- Section 2 types are frozen after W0. Only copy-data may **add** optional fields to `copy/types.rs`, reporting each in `notDone` so the shared owner runs `pnpm typegen`. The testkit API of section 2 is frozen; copy-data may only extend it.
- copy-data's e2e tests depend on copy-schema's SQLite and MySQL planners: written against this contract, they pass once both are in (W2).
- Docker is not available on this PC: copy-it's tests run only in CI; locally they must compile with `cargo check -p bulk-core --features it --tests`.

**W2 (shared):** merge; `pnpm typegen`; `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p bulk-core`, `pnpm --filter desktop lint`, `pnpm --filter desktop test`; palette connection rows get "창 N" chips and `Mod+Enter` 새 창에서 열기 (`palette/**`); fix the seams reported in `notDone`; DESIGN.md §8 parity rows for windows and copy.

## 11. Known limitations

What the implementation deliberately does not do, with the reason. Each is reported to the user where it applies (issue codes in parentheses), never dropped silently.

- **PostgreSQL database and role settings** (`pg_db_role_setting`: `ALTER DATABASE … SET`, `ALTER ROLE … [IN DATABASE …] SET`, e.g. a `search_path` set for the copied owners) are not copied (`PG_DB_ROLE_SETTINGS`, warning, lists them). Role settings are cluster-wide role changes and roles are not copied (non-goal); the target statement guard refuses `ALTER ROLE` / `ALTER DATABASE` in every context (5.2), so BulkDB never writes outside the target database's objects. Apply them by hand after the copy.
- **ICU and builtin default collations** (PostgreSQL 15+ `datlocprovider` `i`, 17+ `b`) are not mapped to a named collation for text columns and partition keys (3.5): the target's own default applies, and `PG_DEFAULT_COLLATION_DIFFERS` (warning) says so when the defaults differ. libc, `C` and `POSIX` defaults are mapped. Mapping an ICU locale to a target collation name is not reliable across ICU versions.
- **Revoked default privileges.** Grants are copied as `GRANT` only (`owners` / `grants` flags): a default `PUBLIC` privilege the source revoked (`EXECUTE` on functions, `USAGE` on types) stays granted on the target. Revoke it by hand when it matters.
- **MySQL routines and events** are not copied (non-goal); each is listed as an `unsupported` item (`MYSQL_ROUTINES_NOT_COPIED`).
- **4-byte characters into an existing utf8mb3 table.** The writer replaces 4-byte characters by `?` for the effective charset of the tables it creates (3.3). For a `truncate` / `append` table that already exists the plan does not read the columns' charset; the server refuses such characters (a strict-mode `INSERT` error, or a LOAD DATA warning that fails the table with `DATA_WARNINGS`), so the table fails instead of changing silently.
- **MyISAM partial chunks.** `left_behind.partial_rows` counts whole committed chunks; a MyISAM table can also keep rows of the interrupted chunk (no transactions). Resume reloads such tables (`RESUME_RELOAD`, or `DESTRUCTIVE_TRUNCATE` when the target holds more rows than the run wrote).
- **Real-server combinations** (MySQL 5.5 → 8 / 9, PostgreSQL 15 → 16 / 17, MySQL 4.0) run only in CI (`copy_it.rs`, `--features it`, crates/core/tests/README.md); the development machine has no docker, so locally they only compile. Local end-to-end coverage is SQLite → SQLite and the in-process fake MySQL 4.0 / 5.5 servers.
- **Hard-linked SQLite files.** Same-file detection for the `SAME_DATABASE` check uses `fs::canonicalize` (symlinks and `file:` URIs resolved; letter case normalised on case-insensitive file systems). Hard links to the same underlying inode are not detected: two paths that are hard links to the same SQLite file would pass the `SAME_DATABASE` check and overwrite the source. Hard-link detection requires comparing `dev + inode` (Unix) or `volume serial number + file index` (Windows) and is not yet implemented.
