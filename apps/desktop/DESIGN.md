# BulkDB desktop: TablePlus-style redesign (binding spec)

Binding for the redesign. Six implementers work in parallel from this file, in the waves of section 9. Where it is silent,
follow CLAUDE.md, then the nearest rule here, then TablePlus. If something is impossible, do the closest thing and report
it. Owner keys: section 9. Recorded deviations (one from CLAUDE.md: 26px rows): section 10.

---

## 1. TablePlus, as researched

Sources: tableplus.com/docs (Toolbar, Left sidebar, Manage connections, Open Anything, Tabs/Workspaces, Query editor, Query
history, Console log, Right sidebar, Shortcut keys), G2 / Capterra reviews. A compact Welcome window (~760x480; every
connection has a tag: local, testing, development, staging, production; driver-first form with Test, Connect, Save); a
workspace with a unified title bar/toolbar and a **centred status pill** (tag, name, driver/version, database, schema,
progress line), no status bar, right sidebar hidden at start, equal-width tabs on a darker strip, preview tabs, table tab
**Data | Structure**, Run with the editor, **Open anything** `⌘P`. It feels native through system fonts (also for object
names), grey segmented controls with a white thumb, raised white buttons, solid-accent selection in the focused list, a
real macOS menu bar, nothing selectable that is not content, one accent with the tag as the only strong colour, hairlines
instead of boxes, hover-only affordances and no list animation.

## 2. Principles

1. **Calm neutral chrome.** Greys and one accent (focused selection, the one primary button per region, focus ring, links,
   progress). Colour means something (tag, job state, danger) and never by hue alone: state also has an icon or text.
2. **Separators, not boxes.** Hairlines between regions and rows; no filled panel headers, cards or panel borders. Shadows
   only on popovers, menus, toasts, dialogs, the Welcome surface, segment thumbs and raised buttons.
3. **Native density.** 13px base, 26px rows (24px menus and section headers, 44px two-line), 40px toolbar, 30px tab bar,
   28px for every inner bar, no status bar, system fonts, tabular numbers. Hangul is never below 12px.
4. **Hover-only affordances.** Tab close, row `⋯`, copy buttons, section chevrons appear on hover or keyboard focus.
5. **Keyboard first.** Every action is in the palette, most have a shortcut (macOS: also in the menu bar); lists use
   arrows, `Enter`, `Esc`.
6. **One connection per workspace.** Welcome when nothing is open; switch via switcher, pill, palette or Welcome.
   Workspaces stay in memory for the session (tabs and SQL survive switching).
7. **Environment always visible.** The pill carries tag + connection name; production is red wherever it appears.
8. **Rust is the state of record.** Job UI renders `job:*` events; the UI may open panels, never change job state.
9. **No data, ever.** No grid, preview or sample rows, also not in tooltips or the palette. Every visible row count says
   추정 (estimated) unless it is an exact count, which shows its date.
10. **Fast and still.** No shimmer, no transition on rows, tabs or menu items, nothing over 120ms, no hover shift.

**BulkDB adaptation.** The result area becomes the **export panel** under the editor, driven by one 28px query bar (`출력 ·
세션 │ 계획 · 검사 · 작업`; 작업 = this tab's last export job). The table tab's toggle is `쿼리 | 구조` (쿼리 = ready
extraction query from `catalog_select_sql`; 구조 = 열 / 인덱스 / DDL / 개요). The right sidebar is the **inspector** (`활동 |
세부 정보`), hidden by default.

---

## 3. Design tokens

### 3.1 CSS custom properties (`src/app/globals.css`)

Components never use hex values. Dark values go under `@media (prefers-color-scheme: dark) {
:root:not([data-theme='light']) {…} }` and `:root[data-theme='dark'] {…}` (same values), as today; `body` keeps an explicit
background. AppShell sets `data-platform="mac" | "windows"` on `<html>` (`lib/platform`); Windows overrides live under
`:root[data-platform='windows']` and its dark variants.

| token | light | dark | use |
|---|---|---|---|
| `--bg-window` | `#ffffff` | `#1e1e20` | editor, tab content, active tab, Welcome surface |
| `--bg-sidebar` | `#f5f5f6` | `#252528` | left sidebar, inspector, Welcome left pane |
| `--bg-toolbar` | `#efeff0` (Windows `#f3f3f3`) | `#2a2a2d` (Windows `#202020`) | toolbar, Welcome backdrop |
| `--bg-tabbar` | `#e4e4e6` | `#1b1b1d` | tab strip |
| `--bg-panel` | `#fafafa` | `#222225` | export panel, console, query bar, table headers |
| `--bg-elevated` | `#ffffff` | `#2c2c30` | popovers, menus, dialogs, palette, toasts (opaque) |
| `--bg-input` / `--border-input` | `#ffffff` / `rgba(0,0,0,.15)` | `rgba(255,255,255,.05)` / `rgba(255,255,255,.12)` | inputs, selects |
| `--bg-control` | `rgba(0,0,0,.05)` | `rgba(255,255,255,.07)` | segmented track, pressed ghost toolbar button only |
| `--btn-bg` / `-hover` / `-pressed` | `#ffffff` / `#f7f7f8` / `#ededee` | `rgba(255,255,255,.10)` / `.14` / `.07` | secondary button |
| `--btn-shadow` | `0 0 0 .5px rgba(0,0,0,.16), 0 1px 1px rgba(0,0,0,.06)` | `0 0 0 .5px rgba(255,255,255,.08)` | secondary button |
| `--seg-thumb` / `--seg-thumb-shadow` | `#ffffff` / `0 0 0 .5px rgba(0,0,0,.10), 0 1px 2px rgba(0,0,0,.10)` | `rgba(255,255,255,.16)` / `0 0 0 .5px rgba(255,255,255,.08)` | active segment (track variant) |
| `--bg-hover` / `--bg-pressed` | `rgba(0,0,0,.045)` / `rgba(0,0,0,.08)` | `rgba(255,255,255,.055)` / `rgba(255,255,255,.09)` | |
| `--bg-selected` | `rgba(0,0,0,.08)` | `rgba(255,255,255,.10)` | selected row, list not focused |
| `--bg-selected-focus` | `var(--accent-fill)` | `var(--accent-fill)` | selected row in the focused list (text `--fg-on-accent`) |
| `--separator` / `--separator-strong` | `rgba(0,0,0,.08)` / `rgba(0,0,0,.15)` | `rgba(255,255,255,.08)` / `rgba(255,255,255,.15)` | hairlines |
| `--fg` / `--fg-secondary` / `--fg-tertiary` | `#1d1d1f` / `#636366` / `#86868b` | `#e9e9eb` / `#a1a1a6` / `#7c7c82` | text; labels, icons, estimates; Latin captions |
| `--fg-on-accent` / `--fg-on-accent-muted` | `#ffffff` / `rgba(255,255,255,.85)` | same | on accent fill / secondary text in focused selection |
| `--accent` / `--accent-hover` | `#1f6feb` / `#1a5fd0` | `#4c8dff` / `#6ea2ff` | links, active icons, progress, focused input border (text colours: never a fill under white text) |
| `--accent-fill` / `--accent-fill-hover` | `#1f6feb` / `#1a5fd0` | `#2f6fe0` / `#2a63cc` | the one primary button per region (hover); focused selection (white text ≥ 4.5:1) |
| `--accent-soft` | `rgba(31,111,235,.12)` | `rgba(76,141,255,.18)` | Welcome mark tile |
| `--danger` / `--danger-soft` | `#d92d20` / `rgba(217,45,32,.10)` | `#ff6b5f` / `rgba(255,107,95,.14)` | red text and tints only (never under white text) |
| `--danger-fill` / `--danger-fill-hover` | `#d92d20` / `#b42318` | `#d43a2f` / `#b9302a` | anything red with white text: `dangerFilled` button (hover), the console error badge (white text ≥ 4.5:1) |
| `--warning` / `--warning-soft` | `#b26a00` / `rgba(230,140,0,.12)` | `#f5b041` / `rgba(245,176,65,.14)` | |
| `--success` / `--success-soft` | `#157f3c` / `rgba(21,127,60,.10)` | `#4cc38a` / `rgba(76,195,138,.14)` | |
| `--focus` | `rgba(31,111,235,.55)` | `rgba(76,141,255,.60)` | focus ring |
| `--env-<tag>-fill` | local `#6e6e73`, development `#2b7a3b`, testing `#6741d9`, staging `#c2410c`, production `#c92a2a` | same | anything with white text (all ≥ 4.5:1 on white) |
| `--env-<tag>` | `var(--env-<tag>-fill)` | local `#98989d`, development `#51cf66`, testing `#9775fa`, staging `#ff922b`, production `#ff6b6b` | dots, coloured text |
| `--env-<tag>-soft` | `color-mix(in srgb, var(--env-<tag>) 14%, transparent)` | same | pill / badge tint |
| `--elev-popover` | `0 0 0 .5px rgba(0,0,0,.12), 0 6px 20px rgba(0,0,0,.12)` | `0 0 0 .5px rgba(255,255,255,.10), 0 8px 24px rgba(0,0,0,.45)` | |
| `--elev-dialog` | `0 0 0 .5px rgba(0,0,0,.14), 0 18px 50px rgba(0,0,0,.22)` | `0 0 0 .5px rgba(255,255,255,.10), 0 20px 60px rgba(0,0,0,.55)` | dialogs, palette, Welcome surface |
| `--scrim` | `rgba(0,0,0,.18)` | `rgba(0,0,0,.35)` | behind dialogs (palette: none) |

Testing is violet so it never reads as the accent blue; development, staging and production sit near success, warning and
danger, so job states always carry an icon (`Check`, `TriangleAlert`, `CircleX`) and the pill's error state an icon plus
text. Syntax (`--syn-*`, light / dark, Xcode-like as TablePlus' default): keyword `#9b2393` / `#fc5fa3`; string `#c41a16`
/ `#fc6a5d`; number `#1c00cf` / `#d0bf69`; comment `#6c7986` / `#7f8c98`; type and builtin `#3900a0` / `#a167e6`;
function `#326d74` / `#67b7a4`; variable and parameter `#643820` / `#ffa14f`; selection `rgba(31,111,235,.20)` /
`rgba(76,141,255,.32)`. Operators and identifiers use `--fg`; all weight 400, comments may be italic. The SqlEditor theme
uses only `--syn-*`, never `--accent`, `--danger`, `--success` or `--warning`.

Sizes: `--h-toolbar: 40px; --h-tabbar: 30px; --h-bar: 28px; --h-row: 26px; --h-row-sm: 24px; --h-row-2: 44px;
--h-control: 26px; --h-control-sm: 22px; --w-sidebar: 240px; --w-inspector: 300px` (use as `h-(--h-bar)`). The 26px row
deviates from CLAUDE.md's 28px on purpose (section 10); implementers cite "DESIGN.md §3.1" in their summaries.

### 3.2 Tailwind 4 `@theme inline`

Every colour token of 3.1 maps to `--color-<name>` without the `bg-` prefix (`--fg-on-accent*` → `--color-on-accent*`,
`--btn-bg*` → `--color-btn*`): window, sidebar, toolbar, tabbar, panel, elevated, input, border-input, control, btn(-hover,
-pressed), seg-thumb, hover, pressed, selected, selected-focus, separator(-strong), fg(-secondary, -tertiary),
on-accent(-muted), accent(-hover, -fill, -soft), danger / warning / success (+ `-soft`), focus, and `env-<tag>`,
`env-<tag>-soft`, `env-<tag>-fill` for all five tags. `--bg-selected-strong` and `--bg-control-hover` are gone.

```css
@theme inline {
  /* …the --color-* mappings above… */
  /* legacy aliases, deleted in wave W3 */
  --color-background: var(--bg-window);    --color-foreground: var(--fg);
  --color-muted: var(--bg-panel);          --color-muted-foreground: var(--fg-secondary);
  --color-border: var(--separator);        --color-accent-foreground: var(--fg-on-accent);
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI Variable Text", "Segoe UI", system-ui,
    "Apple SD Gothic Neo", "Malgun Gothic", "Noto Sans KR", sans-serif;
  --font-mono: ui-monospace, "SF Mono", "Cascadia Mono", "Cascadia Code", Consolas, "D2Coding", "Malgun Gothic", monospace;
  --text-xs: 11px;   --text-xs--line-height: 14px;   --text-sm: 12px;   --text-sm--line-height: 16px;
  --text-base: 13px; --text-base--line-height: 18px; --text-lg: 15px;   --text-lg--line-height: 20px;
  --text-xl: 20px;   --text-xl--line-height: 26px;
  --radius-sm: 4px;  --radius-md: 6px;  --radius-lg: 10px;
  --shadow-popover: var(--elev-popover);  --shadow-dialog: var(--elev-dialog);
  --shadow-btn: var(--btn-shadow);        --shadow-seg: var(--seg-thumb-shadow);
}
```

`text-xs` becomes 11px and `text-sm` 12px (intended). New code uses the new names; aliases keep unmigrated files rendering.

### 3.3 Type, icons, shape, motion, native chrome

- **Type.** Body 13/18 `--fg`; secondary 12px. **Any string containing Hangul is at least 12px**; 11px only for
  Latin/numeric text (tag keys, KeyHints, `≈` numbers, counts). Section captions 12px/600 `--fg-secondary`, no uppercase
  tracking. Weights 400, 500 (names, active tab, active segment), 600 (titles). `tabular-nums` for numbers.
- **Sans vs mono.** Object names are sans 13px everywhere (sidebar, tabs, palette, Welcome, history, inspector, Structure
  이름 column). Mono 12px only for the SQL editor (13px), DDL, the 타입 / 기본값 / 조건 columns, code chips, fingerprints and
  console rows. Paths stay sans, middle-truncated, full path in the tooltip.
- **SQL editor.** Mono 13/20, tertiary gutter without border, active line `--bg-hover`, selection `--syn-selection`.
- **Icons.** lucide, 14px in rows/menus/buttons, 16px in the toolbar, `strokeWidth={1.75}`, `--fg-secondary`; `--accent`
  only when active. No emoji. **Radii**: 4px chips/kbd; 5px segment thumb; 6px controls, track, row selections; 8px menus;
  10px dialogs, popovers, palette, Welcome surface; 999px pills; tabs and job rows none.
- **Selection.** A selected row is `--bg-selected` while its list is unfocused, `--bg-selected-focus` + `--fg-on-accent`
  while it has focus (`:focus-within` on the listbox). Inside a focused selected row, secondary, tertiary, tag-key and
  estimate text and icons become `--fg-on-accent-muted`; EnvTag chips keep their fill; the Welcome driver badge switches to
  `--env-<tag>-fill` with white text.
- **Focus.** `:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px }` for buttons, segmented controls and
  links. Inputs: border `--accent` + `box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 35%, transparent)` in both
  themes. Rows in roving-focus widgets (listbox, tablist, tree, palette results) get `outline: none`.
- **Motion.** 80ms colour transition on buttons only; none on rows, tabs, menu items. Popover/dialog/sheet enter 120ms
  (opacity + 4-8px), exit instant; indeterminate progress may loop 1.2s. `prefers-reduced-motion` disables all of it.
- **Scrollbars.** Thumb transparent until the container is hovered or scrolling, then `--fg-tertiary` at 45%; 8px wide
  with a 2px transparent border (6px visible); transparent track; `scrollbar-gutter: stable` on lists.
- **Native chrome (globals.css + AppShell).** `html, body { user-select: none; -webkit-user-select: none; cursor: default;
  overscroll-behavior: none }`; `input, textarea, .cm-editor, [data-selectable] { user-select: text }`; error details,
  console rows, DDL, raw plans, paths and fingerprints carry `data-selectable`; `img, a { -webkit-user-drag: none }`.
  Buttons, rows and tabs use `cursor: default`, never `pointer`. A global `contextmenu` listener calls `preventDefault()`
  unless the target is editable, inside selected text, or inside a component with its own ContextMenu.
- **Feedback.** `CopyButton` swaps to `Check` (`--success`) for 1.2s with aria-live "복사됨"; menu copies give no toast.
  Toasts only for background outcomes (rescan count, import, duplicate, metadata reload).
- **Don't**: zebra rows, coloured panel headers, panel borders, cards, gradients, more than one filled primary button per
  region, accent-filled segment thumbs, centred body text, tooltips without delay (Radix default; 300ms in the toolbar).

---

## 4. App structure

```
AppShell (layout/AppShell.tsx: data-platform, --sheet-top, contextmenu guard, registerBuiltinActions)
├─ AppEvents                                  (activity; outside the key={lang} subtree, never remounted)
├─ mode 'welcome'   → WelcomeBackdrop (bg-toolbar) → WelcomeScreen({ headerEnd })            (welcome)
│                     headerEnd = <JobChip variant="welcome"/> <LogStatusButton/>             (activity)
├─ mode 'workspace' → Workspace(connId)       (layout/Workspace.tsx)
│    ├─ WorkspaceToolbar 40px: switcher · StatusPill · JobChip, refresh, palette, LogStatusButton, inspector, mock
│    └─ SidebarFrame │ main(WorkspaceTabBar 30px + TabContent) │ InspectorFrame (closed by default)
├─ ConsoleFrame(ConsoleDrawer) when ui.logOpen (full width at the window bottom, both modes)
└─ once: CommandPalette, ConnectionDialog, ImportDbeaverFlow, SettingsDialog, AboutDialog, PreflightSheet,
   DbPasswordDialog, HostKeyDialog, SshPasswordDialog, ConfirmHost, Toaster
```

No app-wide status bar: the pill is the global status location, the JobChip the global job indicator. Always starts on
Welcome. Window minimum 1024x680. Sizes (ui store, persisted per viewer): sidebar 240 (180-480), inspector 300 (260-520),
console 220 (120-600), export panel 280 (120-640, at most 50% of the tab content). `TabContent` keeps every query-target
tab of the active workspace mounted and hides inactive ones with `hidden` (CodeMirror undo, selection and scroll survive;
`QueryTabView` calls `view.requestMeasure()` when shown); structure and history tabs unmount when inactive.

---

## 5. Screens

### A. Welcome / connection manager (`components/welcome/**`, welcome)

```
  bg-toolbar backdrop (macOS: top 32px = drag region under the traffic lights)
  ┌──────────────────────┬─────────────────────────────────────────────────────────┐
  │        ┌────┐        │ ⌕ 연결 검색…             Ctrl+F   ⟳ 41%  ▤  정렬 ▾   + │ 44
  │        │ ⛁  │        ├─────────────────────────────────────────────────────────┤
  │        └────┘        │ ┌──┐ prod-sales  production                     ●  ⋯   │ 44
  │        BulkDB        │ │Pg│ db1.prod.internal:5432 · sales · SSH deploy@…     │
  │     v0.1.0  mock     │ ┌──┐ staging-sales  staging              열림 · 탭 2   │
  │ ＋ 새 연결     Ctrl+N │ │Pg│ db1.stg:5432 · sales                              │
  │ ⇪ DBeaver에서 가져오기…│                                                         │
  │ 설정 · BulkDB 정보     │                                                         │
  └──────────────────────┴─────────────────────────────────────────────────────────┘
```

- Surface `width: min(820px, 100% - 64px)`, `height: min(540px, 100% - 64px)`, centred, `bg-window`, radius 10,
  `shadow-dialog`, on the AppShell backdrop; no status bar (the console drawer, when open, sits under the backdrop).
- Left pane 260px `bg-sidebar`: mark (lucide `Database` 28px on a 56px `--accent-soft` rounded square), name 20px/600,
  version (`app_info`) 11px tertiary + `mock` chip when `!inTauri()`; 32px ghost rows with KeyHint: 새 연결, DBeaver에서
  가져오기…; footer links 설정 · BulkDB 정보.
- Header 44px: `SearchField` (fills; `Mod+F`), `headerEnd`, a `정렬` ghost menu (radio 보기: 이름순 / 태그별 / 드라이버별
  = `ui.welcomeGroupBy`; 드라이버 다시 검색), icon-only `+` (새 연결). A printable key typed while nothing is focused focuses
  the search on keydown without `preventDefault`, skipped when `e.isComposing` or `e.key === 'Process'` (known limit: the
  first Hangul jamo can be lost; `Mod+F` works).
- Rows 44px two-line (`useListNavigation`, selection per 3.3). Driver badge 28x28 radius 6 with a 2-letter code (`Pg My Ms
  Lt Or Od`, else the name's first two letters), bg `--env-<tag>-soft`, text `--env-<tag>` (no tag: control + secondary).
  Line 1: name 13/500, tag key (lowercase Latin 11px, `--env-<tag>`); right: test status dot (only after a test this
  session; tooltip = error), `열림 · 탭 N` when its workspace is in memory, `드라이버 필요` chip, `⋯` on hover. Line 2
  (12px secondary): `host:port · database · SSH user@host` (SQLite: the path, middle-truncated). Grouping: none (name) /
  tag (production, staging, testing, development, local, no tag) / driver.
- Keys (WelcomeScreen binds them with `useGlobalShortcuts` while mounted; KEYMAP welcome rows are labels only): `↑↓ Home
  End` select (`connections.select`); `Enter` / double-click = `openWorkspace(id)`; `Delete` or `Mod+Backspace` = delete
  (confirm; KeyHint `⌘⌫` on macOS, `Delete` on Windows); `Esc` with text clears the search (consumed). Menu and `⋯`: 연결,
  연결 테스트, 편집…, 복제 | 작업공간 닫기 (when open) | 새 연결…, DBeaver에서 가져오기…, 드라이버 다시 검색 | 삭제 (danger).
  Fuzzy search over name, host, database, driver, tag. Empty: "아직 연결이 없습니다" + [새 연결] [DBeaver에서 가져오기…].
  복제 = `duplicateConnection(id)` (7.5) + toast "비밀번호는 복사되지 않았습니다".

### B. Connection form (`dialogs/ConnectionDialog.tsx`, welcome)

A 560px `Sheet`. New: step 1 driver picker, step 2 form. Edit: form only.
- Step 1: `SearchField` + 3-column grid of 64px driver tiles (badge, `DriverInfo.name`, tier caption 내장 / 벤더 클라이언트
  / ODBC / 플러그인); unavailable drivers dimmed with "설치 필요" but selectable (saving stays blocked as today); arrows,
  `Enter`/double-click = next; footer 드라이버 다시 검색 (left), 취소, 다음.
- Step 2: 112px right-aligned labels, 26px controls, 8px gaps; name + `TagPicker` on row 1; "변경" next to the driver
  reopens step 1 (new) or shows a Select (edit). Fields, secrets (sent once, cleared, `(변경 없음)`, keychain =
  `savePassword`, clear stored password / SSH secret, `refreshConnection` on edit), per-connection 기본 출력 폴더, SSH
  section (collapsible, `Switch` header, auth `SegmentedControl` 키 / 비밀번호 / 에이전트, key file, passphrase,
  compression, clear secret) and the unavailable-driver `Notice` (hint, install link + failure text, rescan) work exactly
  as today.
- Footer: [테스트] + inline result (`✓ {driver} {version} · {ms}`, `SSH 경유`, `work_mem 권장 {x}`; failure in danger
  text, full error in tooltip); right [취소] [저장] [연결] (primary). Test = save then test. Connect = save; if
  `savePassword` is off and a password was typed, `connections.test(id, password)` first (unlocks the session); close;
  `openWorkspace(id)`. Editing an open connection: primary [저장], [연결] hidden. Every save of an existing connection
  calls `afterConnectionSaved(id)` (7.5).

### C. Workspace (`layout/*`, ui-foundation; slot content by owners)

```
┌──────────────────────────────────────────────────────────────────────────────────────────────┐
│●●●  ◧ ▣▾        ( PRODUCTION │ prod-sales : PostgreSQL 16.2 : sales : public )  ⟳41%  ⟳ ⌘ ▤ ◨ │40
├──────────────────┬───────────────────────────────────────────────────────────────────────────┤
│ [ 항목 │ 기록 ]   │      ▦ orders      │      쿼리 1 ●      │      쿼리 2 ⟳      │           + │30
│ ⌕ 검색  Ctrl+⇧+F  ├───────────────────────────────────────────────────────────────────────────┤
│ 테이블 24    추정  │  1  SELECT o.id, o.created_at, o.total                                     │
│ ▦ customers ≈1.2M│  2  FROM sales.orders o                                                    │
│ ▦ orders   ≈48.2M│  3  WHERE o.created_at >= '2026-01-01'                                     │
│ 뷰 3             ├───────────────────────────────────────────────────────────────────────────┤
│ ◫ v_daily        │ SQL▾ │ 출력 세션 │ 계획• 검사✓ 작업⟳   CSV · BOM → D:\exports\prod_…  [■ 중지] │28
├──────────────────┤ (export panel: collapsed by default, ≤ 50% of the tab)                      │
│⛁ sales ▾ ⌗ public ▾ ⟳│                                                                        │28
└──────────────────┴───────────────────────────────────────────────────────────────────────────┘
```

**Toolbar** (`WorkspaceToolbar`, 40px `bg-toolbar`, bottom hairline, CSS grid `1fr auto 1fr` so the pill is truly centred;
28px ghost buttons, 16px icons). macOS (Overlay title bar, section 10): `padding-left: 78px` when `isMac()`; empty space
carries `data-tauri-drag-region`. Windows: native caption above, **no** drag region, `--bg-toolbar` = caption colour.
- Left: sidebar toggle (`PanelLeft`, `Mod+0`); icon-only connection switcher (`Server` + chevron, tooltip "연결 전환 ⌘⇧O";
  menu: connections with EnvTag dot and `열림`, then 모든 연결… (Welcome), 새 연결…; pick = `openWorkspace`).
- Centre: `StatusPill` (max 520px, middle-truncated): `TAG │ {name} : {DriverInfo.name} {version} : {execDb}`, plus `:
  {schema}` while `scope.database === execDb`. `execDb` = `execDatabase(connId)` (7.5), where queries run, never the
  browse scope. Tag chip `--env-<tag>-fill`, white 11px/600 uppercase key; pill bg `--env-<tag>-soft` (no tag:
  `--bg-control`). While a job of this connection is active the text after the name becomes `⟳ orders 41% · ETA 3:12`
  (warned: `⚠ 경고 확인 필요`) with a 2px progress line along the pill's bottom; tag chip and name always stay. States:
  connecting (spinner, "연결 중…"), ok, error (`CircleAlert` + "연결 실패", never a bare red dot).
- Pill click → `Popover` (`layout/StatusPillPopover.tsx`): name + EnvTag; this connection's active jobs + [활동 보기];
  드라이버, 서버 버전, 호스트, 실행 데이터베이스, 탐색 중 (when different), 사용자, SSH, 서버 RAM (추정); warning when
  `ServerInfo.canWrite` ("이 계정은 쓰기 권한이 있습니다. 읽기 전용 계정을 권장합니다"); error text; buttons 연결 테스트, 연결
  편집…, 다른 연결…, 작업공간 닫기; footer link 앱 설정….
- Right: `JobChip` (only while jobs exist), refresh (`RotateCw`, `Mod+R`), palette (`Command`, `Mod+P`),
  `LogStatusButton` (`Mod+J`), inspector (`PanelRight`, `Mod+\`), `mock` chip when `!inTauri()`. No settings button
  (`Mod+,`, palette, menu, pill popover, Welcome footer) and no Run group (it lives in the query bar).

**Sidebar** (frame `layout/SidebarFrame.tsx`: `bg-sidebar`, right hairline, resizable, 28px top row `SegmentedControl`
(track, sm) 항목 | 기록 = `ui.sidebarSegment`, then slot `Sidebar` or `HistoryList`; items `components/sidebar/**`, sidebar):
- `SearchField` (placeholder `검색`, KeyHint `mod+shift+f`): fuzzy over the current schema; `Esc` with text clears it
  (consumed); `↓` enters the list.
- Sections by `TableKind`: 테이블 (table + partitioned, the latter with `Layers`), 뷰, 구체화된 뷰, 외부 테이블, 기타; hidden
  when empty. 24px **sticky** header: 12px/600 caption + count, chevron on hover, collapse persisted in
  `ui.collapsedSections['items:<kind>']`. **Every** section with an estimated row shows a right-aligned `추정` caption
  (12px secondary), visible while scrolling.
- 26px rows: icon (`Table2`, `Layers`, `View`, accent `View` for materialized, `Plug`), name, right `≈48.2M` (11px
  `--fg-secondary`); an exact count from `catalog.exactCounts` shows `48,213,004 · 10-27`. Tooltip: `schema.name`,
  comment, "추정 행 수 48,213,000 (통계 기반 추정)" or "정확한 행 수 … (시각)", total size; aria-label says 추정 / 정확.
- Click = select + open the **preview** table tab; double-click / `Enter` = pinned; `↑↓ Home End` move the selection only
  (`useListNavigation`; no loads anywhere, the inspector included). Stored in `workspace.selection[connId]`; `SidebarItem`
  has `id` + `aria-selected`, the list `aria-activedescendant`.
- Context menu: 열기 (새 탭), 구조 보기, 추출 쿼리 만들기, 정확한 행 수 세기 | 열 목록 복사, 이름 복사 | 새로 고침 (via
  `lib/table-actions` / `lib/actions`); 추출 쿼리 만들기 is disabled with the `canTargetTable` reason (7.5).
- Bottom bar 28px: database (`Database`), schema (`FolderTree`), refresh (= `catalog.refresh`: invalidate, then reload).
  Searchable `Popover`s: databases with size, `(현재)` = `isCurrent`, `(실행)` = execDb, inaccessible struck-through +
  disabled ("접근할 수 없음"), encoding/collation tooltip; schemas with table counts; choice → `workspace.setScope`. While
  `scope.database !== execDb`, a persistent warning-soft 24px row above the bar reads "탐색 전용 · 쿼리는 {execDb}에서 실행".
- `ui.sidebarRequest` (`focusSearch` | `database` | `schema`, from `Mod+Shift+F`, `Mod+K`, the palette) is consumed in an
  effect and cleared. Scope resolution lives in the workspace store (`ensureScope`); the sidebar renders `workspace.scope`
  and calls `setScope`.
- States: spinner row "불러오는 중…"; inline error + [다시 시도]; 데이터베이스 없음 / 스키마 없음 / 테이블 없음; "비밀번호
  필요" + [비밀번호 입력] when `needsSessionPassword` (reason `browse`).

**Tab bar** (`layout/WorkspaceTabBar.tsx` with `TabBar`/`TabItem`; 30px `bg-tabbar`, bottom hairline): tabs `flex: 1 1 0;
min-width: 120px` (no max), centred 12px title + 14px icon, horizontal scroll only below the minimum. Active tab
`bg-window`, `--fg` 500, 1px `--separator` sides; inactive `--fg-secondary`, no fill (hover `bg-hover`). Close button left
on macOS, right on Windows, on hover or when active; dirty doc = dot in its place until hover; busy doc (`busy` or its
`lastJobId` job active) = 10px spinner; preview = italic. `+` is a plain icon button (click = new query tab; right-click:
새 쿼리, SQL 파일 열기…, 전체 기록). Double-click empty strip = new query tab; double-click preview = pin; middle-click =
close; drag = reorder. Context menu: 닫기, 다른 탭 닫기, 오른쪽 탭 닫기, 모두 닫기, 이름 바꾸기 (inline; `Esc` cancels and is
consumed; empty = default), 고정 (preview), 다른 연결에서 열기… (query targets). Titles: query `customTitle ?? doc.name ||
"쿼리 N"`; table = table name (tooltip `schema.name @ database`); history "기록". No tabs: empty state with KeyHints.

**Inspector** (`layout/InspectorFrame.tsx`; `bg-sidebar`, left hairline, resizable): **closed by default**
(`inspectorOpen: false`, persisted once toggled); opened by `Mod+\`, the toolbar button and `showActivity()`. 28px header
`SegmentedControl variant="tabs"`: 활동 (badge = active job count) | 세부 정보 → `ActivityPanel variant="inspector"` /
`TableInspector`. `view.toggleInspector` opening it with a table tab active selects 세부 정보; `showActivity` selects 활동.
TableInspector target: the active table tab's table (`source: 'tab'`) unless that tab shows Structure 개요 (then the
sidebar selection, else the hint "개요 탭에 표시 중"); with no table tab, the sidebar selection (`source: 'selection'`).

### D. Query tab (`components/query/QueryTabView.tsx`, query)

```
┌ editor (CodeMirror, fills) ────────────────────────────────────────────────────────────────────────────────┐
├ 28px: SQL▾ │ 출력 세션 │ 계획• 검사✓ 작업⟳    CSV · BOM → D:\exports\…    검사 중…   [검사] [계획] [▶ 실행 ⌘↵] │
│ export panel (collapsed for new docs; splitter = ui.exportPanelHeight, ≤ 50% of the tab)                     │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- Editor: `SqlEditor connId={doc.connId}` (dialect + completion), `onRun` / `onExplain` / `onCheck` (Mod+Shift+Enter).
  File mode: path + [찾아보기…] + [불러오기], size · modified, read-only editor.
- Query bar (`bg-panel`, top hairline), left to right: ghost mode menu `SQL ▾` / `파일 ▾` (SQL 직접 입력, SQL 파일 열기… ⌘O,
  다른 연결에서 열기…); segments (`SegmentedControl variant="tabs"` sm) 출력 · 세션 │ 계획 · 검사 · 작업 (separator between
  the groups; badges: 계획 warning dot, 검사 ✓ / ● from `lastCheck`, 작업 spinner while active and warning dot when
  warned); output summary (12px secondary, middle-truncated, full pattern in the tooltip, click = 출력; hidden below 760px);
  busy caption (검사 중… / 계획 계산 중… / 시작 중… / 불러오는 중…); run group `[검사] [계획] [▶ 실행 ⌘↵]` (22px; 실행 is the
  bar's only primary button; 검사 / 계획 secondary, icon-only below 560px; 실행 spins on `preflight` / `starting`).
- **Stop.** While job `doc.lastJobId` is pending / preflight / running / finishing (per `job:*`), 실행 becomes `■ 중지`
  (ghost, danger text) → `askCancel(jobId)`; while `ui.cancelConfirmJobId === jobId` the group shows "취소할까요? [예]
  [아니요]" (the Activity confirm state; 예 = `cancelJob`). Rust decides; the button only follows events.
- Errors: one-line danger `Notice` under the bar (dismiss = `clearError`).
- Panel: new docs start collapsed; clicking the active segment (`onActivate`) or `` Ctrl+` `` toggles. The store expands
  on Explain → 계획, Check → 검사, job started → 작업; QueryTabView expands to 작업 when the job enters `warned`.
  - 출력: first row 이름 (label "이름 ({name})", feeds the file pattern), then every Output option of today (parity row
    35, [Excel 프리셋] + hint); two columns from 900px, 120px labels. 세션: today's five fields (work_mem hints 64MB, 256MB,
    1GB), those not in `driver.sessionOpts` disabled with a tooltip. 계획: today's `PlanSummaryView` (estimated rows
    labelled 추정, `PlanTree` + warnings + locate, raw plan). 검사: `쿼리 정상 · {KIND}` or issues with code chips.
  - 작업: job `doc.lastJobId`: `StateChip`, `ProgressBar`, rows · bytes · MB/s · rows/s · elapsed · ETA, server wait /
    bottleneck, files (basename → `revealPath`), error, notes, "전체 활동 보기". In `warned`: strip "경고 N건 확인 필요" +
    [검토] → `reviewWarnings(jobId)` (only opens the sheet). Otherwise read-only.

### E. Table tab: Structure | Query (`layout/TableTabFrame.tsx` + slots)

```
┌ 36px: orders  table  @ sales           열   인덱스   DDL   개요            추출 쿼리   정확히 세기   ⟳ │
│  #  이름          타입          NULL       기본값          키      설명                            │
├ 28px: [ 쿼리 │ 구조 ]                                              ≈48.2M행 (추정) · 6.1 GB          │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- `TableTabFrame` (ui-foundation): bottom bar `SegmentedControl` (track, sm) 쿼리 | 구조 (default 구조; 쿼리 disabled with
  the `canTargetTable` reason) + `RowEstimate full` + total size. 구조 → `StructureTabView`; 쿼리 → `QueryTabView tabId`
  (doc lifecycle: `tabs.setTableView`, 7.1).
- `StructureTabView` (structure): 36px content header (name 13/600, kind chip, `@ database`); segments `variant="tabs"` 열
  | 인덱스 | DDL | 개요; right 추출 쿼리 (`openExtractionQuery`, disabled per `canTargetTable`), 정확히 세기 (`countExactly`,
  spinner while counting), refresh details. Tables: 26px rows, sticky `bg-panel` header, hairline rows, no vertical lines.
  - 열: #, 이름 (sans, key icon for PK), 타입 (mono secondary), NULL, 기본값 (mono), 키 (PK / UQ / FK chips, FK target
    tooltip), 설명; `CopyButton` 열 목록 복사. 인덱스: 이름, 열, 종류, 고유, 조건 (`WHERE …` mono warning), 포함, 크기, 스캔;
    empty "인덱스 없음". DDL: `CodeBlock` (ui; never query's SqlEditor) + `CopyButton` DDL 복사.
  - 개요: 추정 행 수 `≈ 48,213,000 (추정)`; 정확한 행 수 `48,213,004 · 10-27 13:02` (tooltip "13:02:11에 12.3초 동안 셈")
    or `—` + [정확히 세기]; 크기 합계 / 테이블 / 인덱스; 종류; 설명; 열 수; 인덱스 수. Last exact count loads on mount
    (`loadLastExactCount`); a newer `job:done` wins (store logic unchanged).

### F. Activity and history

**`ActivityPanel`** (activity): 실행 중 N (newest first) and 최근 결과 (5) + [지우기] (`clearFinished`); empty "실행 중인
작업 없음". Each job is a **flat block** on `bg-sidebar`: padding 10px 12px, 1px `--separator` between jobs, no fill, radius
or coloured border; hover = `bg-hover` + the cancel / remove button. State shows through the StateChip and kind icon.
- Header: kind icon (`FileDown` / `Calculator`), label 13/500, `StateChip` (9 states; spinner on pending / preflight /
  running / finishing; done `Check`, failed `CircleX`, cancelled `Ban`); hover button cancel (`Square`, active) or remove
  (`X`, terminal). Line 2: connection · start.
- running/finishing: 3px `ProgressBar` across the content width (determinate from elapsed + ETA, else indeterminate;
  finishing always indeterminate); `19.8M행 · 1.1 GB · 98 MB/s · 412K행/s · 경과 4:51 · 남은 시간 3:12` + phase + file count
  (`formatRowRate`, `formatDuration`; may wrap onto a second 12px secondary line; finishing: elapsed, no ETA); server line
  (inspector variant): `서버 {state} · 대기 {waitEvent} · 병목 {네트워크|서버}`, temp files, pid.
- Cancel: inline "이 작업을 취소할까요? [예] [아니요]" (`ui.cancelConfirmJobId`), gone once inactive. warned: warning-soft
  strip (radius 6, inset) "경고 N건 확인 필요" + [검토] (`reviewWarnings`).
- Terminal: error strip (danger; cancelled = warning) with code, message, detail; exact count; `행 · 크기 · 시간 · 검증`;
  files (basename + folder button → `revealPath`); notes. The two strips are the only tinted elements.

**`JobChip`** (activity, `monitor/JobChip.tsx`): 28px ghost button, rendered only while the jobs store holds jobs. Active:
spinner + the `selectPrimaryActiveJob` job's `41%` (or phase) + `+N`; warned: `TriangleAlert` + "경고 확인 필요"; only
terminal jobs: a quiet `Check` / `CircleX`. Tooltip: label, rows, ETA. Click → `showActivity()`. Variant `welcome` renders
`ActivityPopover` (`monitor/ActivityPopover.tsx` = `Popover` + `ActivityPanel variant="popover"`) anchored to itself;
opened by `showActivity` it takes no focus (`initialFocus={false}`); closing it clears `cancelConfirmJobId`.

**`HistoryList`** (structure, sidebar 기록): `SearchField` (label, SQL, files) over the loaded page; filter `[이 연결 | 전체]`
(이 연결 = `history.load(connId)`, default; 전체 = `history.load(null)`); 44px rows: state dot + label / `시각 · 행 · 크기`
(전체: other connections' rows show the connection name). Click = `workspace.openFromHistory(entry)` (another connection's
entry switches workspaces); hover 다시 실행 (`{ run: true }`); menu: 새 탭에서 열기, 다시 실행, SQL 복사, 출력 폴더 열기
(first file), 전체 기록 보기 (`tabs.openHistory`). **`HistoryView`** (history tab): connection `Select` (전체 + every saved
connection; reloads through `history.load`), search, refresh + table (다시 실행 column first, 시작, 연결, 이름 with SQL
tooltip, 상태, 행, 크기, 시간, 파일 / error); row click = open in a new query tab.

### G. Console drawer (`ConsoleFrame` ui-foundation, `ConsoleDrawer` activity)

Full width at the window bottom (both modes), `bg-panel`, top hairline, resizable (`ui.logHeight`); `Mod+J`,
`LogStatusButton`, or auto-open on error (logs store, unchanged). Header 28px: "콘솔", count, level `Select`, clear
(`Trash2`), close. Rows mono 12/18: time (tertiary) · LEVEL (44px; trace/debug tertiary, info `--fg-secondary`, warn
warning, error danger) · `[jobId]` · target (from 1280px) · message (wraps); error rows `--danger-soft`; sticks to the
bottom unless scrolled up. `LogStatusButton`: 28px icon button `SquareTerminal` + error badge (`bg-danger-fill`, solid
while `errorFlag`, 50% after ack), tooltip "콘솔 (KeyHint)"; click toggles + acks. Every way of opening or closing the
console acks (`Mod+J` / `view.toggleConsole` both ways, the header close, Esc in the drawer).

### H. Command palette (`components/palette/CommandPalette.tsx`, activity)

`Mod+P` toggles (both modes); `Mod+Shift+O` = scope `connections`. 600px wide, 12vh from top, `bg-elevated`,
`shadow-dialog`, radius 10, no scrim. 40px input (15px, search icon, scope chip; Backspace on empty removes it); prefixes
`>` actions, `@` connections. Grouped results (12px/600 captions), 28px rows: icon, title with matches in 600
(`Highlight`), secondary (schema / host / time); the KeyHint (`Ctrl+↵ 새 탭` for tables, else the action shortcut) shows
right-aligned on the selected row only; no footer; max 10 visible.
- Sources: 테이블 (catalog store, current scope, `loadTables` if missing; rows show `≈48.2M 추정` via `RowEstimate compact
  showLabel`), 연결, 기록 (this connection first), 동작 (`getActions()` where `enabled()`; table actions carry their target
  in the title, "정확히 세기: sales.events", and need one). Welcome: 연결 + 동작. Empty query: tables opened this session on
  this connection, then open connections, then recent actions.
- Pick mode (`ui.palette.pick = { kind: 'openInConnection', tabId }`): other connections with EnvTag chips; `Enter` →
  `openQueryInConnection(tabId, connId)`.
- `↑↓` / `Ctrl+N/P`; `Enter`: table → pinned table tab, connection → `openWorkspace`, history → `openFromHistory`, action →
  close, then `runAction(id)` on the next animation frame; `Mod+Enter` table in a new tab, connection in a new window
  (`window.openConnection`, palette-local; the selected row shows `Ctrl+↵ 새 창`); `Tab` completes; `Esc` clears, again
  closes. `lib/fuzzy`.
- Connection rows (not in pick mode) carry a quiet "창 N" chip (`bg-control`, 18px, like Welcome's) for each **other**
  window of the process that shows the connection (`otherWindowsWith`, docs/db-copy.md 1.1); clicking it closes the
  palette and focuses that window (`window_focus`). Mouse only: the keyboard stays in the input.

### I. Dialogs, sheets, prompts (one style)

`Dialog` (centred, 15vh from top) and `Sheet` (top `var(--sheet-top)`: 48px in workspace mode, 16px on Welcome) share
chrome: `bg-elevated`, radius 10, `shadow-dialog`, `--scrim`, 44px header (title 15/600, optional 12px description), body
16px 20px, footer hairline with right-aligned buttons (Cancel secondary, primary rightmost, optional left slot), no corner
X. **Esc, scrim click and Cancel all dismiss, and a dismiss always sends the matching backend answer.** Widths sm 420, md
560, lg 720, xl 880.
- **PreflightSheet** (query, lg): "실행 전 점검" + connection `EnvTag` + "차단 N건 · 경고 N건"; 차단 (danger rows, no ack),
  경고 (code chip, severity, message, detail, 권장, "확인했습니다" checkbox), 예상치 (rows (추정), size, output path `(+N)`,
  free disk, work_mem + 권장, 서버 RAM, server), 서버 세션 (24px: pid, user, app, state, wait, elapsed; blocking rows
  danger; query tooltip). Footer: elapsed left; [취소] [시작 | 계속] disabled until all warnings are acknowledged, disabled
  + tooltip when blocked; undismissable while starting; errors on top. Sheets queue (`extract.preflightQueue`). `kind:
  'resume'` follows `jobs[jobId].summary.state`: once it is no longer `warned`, an info Notice "작업 상태가 {state}(으)로
  바뀌었습니다" shows and 계속 is disabled (only 닫기 remains).
- **SettingsDialog** / **AboutDialog** (activity, md): today's sections and fields (parity rows 50, 51) in 5.I chrome;
  Settings footer [BulkDB 정보] left, [닫기] [저장] + saved notice; About ("BulkDB 정보", `ui.aboutOpen`, no props) footer
  [정보 복사] left, [닫기].
- **Prompts** (activity, Sheet sm). HostKey: warning `Notice` with the `hostkey.body` text; host, key type and fingerprint
  in a mono box; the raw `prompt.message` in a scrollable mono `<pre>` (max 160px; it tells a first connection from
  "REMOTE HOST IDENTIFICATION HAS CHANGED"); [거부] [수락]; Esc, scrim and 거부 call `tunnel_confirm_hostkey(promptId,
  false)`, resolved in `finally` as today; queue oldest first. SshPassword (password / passphrase for user@host, hint,
  [확인]; Esc = the cancel answer). DbPassword ({user} on {name}, hint, [연결]): head of `ui.dbPasswordPrompts`, keyed by
  `prompt.id`; Esc = cancel (drops the prompt with its requests); resume per 7.1.
- **ImportDbeaverFlow** (welcome, Dialog lg): on `ui.importDbeaverOpen` picks the file, parses, shows the file path in the
  header; a parse failure shows a danger `Notice` with the error. Rows: checkbox (default on for ok and driverMissing,
  unsupported disabled), status chip (지원 안 함 / 드라이버 필요 / 이미 있음 / 가능); saved rows show ✓ and are locked, so a
  retry imports only the failures. Password note; [N개 가져오기] (N = selected and not done); toast + failures; close
  resets.
- **Confirm** (`askConfirm`, Sheet sm) replaces every `window.confirm`.

---

## 6. Keyboard map

`Mod` = Cmd on macOS, Ctrl on Windows; `Ctrl` = physical Ctrl. Global shortcuts never fire inside `role=dialog` (dialogs,
sheets, palette, a popover with focus inside). CodeMirror keeps its default keymap; the editor routes only Mod+Enter, Mod+E
and Mod+Shift+Enter. "menu" = also a macOS menu item.

| keys | action id | scope |
|---|---|---|
| `Mod+P` / `Mod+Shift+O` | `palette.open` / `palette.connections` (switch connection) | always (P: menu) |
| `Mod+N` / `Mod+,` / `Mod+J` | `connection.new` / `app.settings` / `view.toggleConsole` | always (menu) |
| `Mod+Shift+N` | `window.new`: an empty Welcome window (File → 새 창 ⇧⌘N; docs/db-copy.md 1.1). `window.openConnection` (새 창에서 열기: Welcome-local / palette-local `Mod+Enter`), `app.newInstance` (새 BulkDB 실행) and `copy.open` (데이터베이스 복사…) have no global key | always (menu) |
| `Esc` / `Mod+.` | `jobs.cancelAsk` (precedence below) | always; Esc in inputs only when empty; `Mod+.` in inputs too (menu) |
| `F5`, `Mod+R`, `Mod+Shift+R` | `catalog.refresh`: workspace = invalidate + reload; welcome = `connections.load()`; always `preventDefault` (no WebView reload), also inside dialogs and IME composition, where the action does not run (`useWebViewAcceleratorGuard`, capture phase, which also guards `Mod+P` / `Mod+Shift+P` print and `Ctrl`/`Shift+F5`; it leaves Cmd combos to the native menu inside Tauri on macOS) | always (⌘R: menu) |
| `Mod+F`; `↑ ↓ Home End Enter`; `Delete`, `Mod+Backspace` | Welcome search; move / open; delete (confirm) | welcome (WelcomeScreen-local; label rows) |
| `Mod+T` / `Mod+W` / `Mod+Shift+T` | `tabs.newQuery` / `tabs.close` / `tabs.reopenClosed` | workspace (T, W: menu) |
| `Mod+1…8`, `Mod+9`; `Ctrl+Tab`, `Ctrl+Shift+Tab` | tab N / last tab; next / previous tab | workspace |
| `Mod+Enter` | `query.run`; table tab in Structure view → `tabs.setTableView(id, 'query')` (a second ⌘↵ runs; no-op when 쿼리 is disabled); history tab or no tab → `tabs.newQuery` | workspace (menu) |
| `Mod+E` / `Mod+Shift+Enter` | `query.explain` / `query.check` | workspace (menu) |
| `Mod+O` | `query.openFile` (into the active query doc, else a new tab) | workspace (menu) |
| `` Ctrl+` `` | `view.toggleExportPanel` | query target |
| `Mod+K` | `catalog.switchDatabase` "탐색할 데이터베이스 선택" / Browse database (`requestSidebar('database')`) | workspace |
| `Mod+Shift+F` | `sidebar.focusSearch` (`requestSidebar('focusSearch')`) | workspace |
| `Mod+0` / `Mod+\` | `view.toggleSidebar` / `view.toggleInspector` | workspace (menu) |

**Esc precedence.** (1) A component whose Esc does something local calls `e.preventDefault()`: SearchField with text
(clears), the tab rename input (cancels), a list with an active filter, a Popover with focus inside (closes), Radix menus;
CodeMirror's keymap (completion close, selection collapse) runs before the window listener. (2) The global handler skips
`defaultPrevented` events and targets inside `.cm-tooltip-autocomplete`; inside `.cm-editor` it fires whatever the editor
holds (decided before any text check: `.cm-content` is contenteditable and its `textContent` is never empty, it holds the
SQL or the placeholder widget); in any other text input it fires only when the input is empty (`editableHasText`, which
ignores `aria-hidden` widgets). (3)
`jobs.cancelAsk` dismisses an open cancel confirm, else asks for the newest active job (`askCancelLatest`) and calls
`showActivity()`, which opens the inspector (welcome: the popover) only if closed and never moves focus. `Mod+.` runs the
same action, also in inputs.

**Matching (`lib/shortcuts.ts`).** Each binding has a `key` and a physical `code` (`Backquote`, `Backslash`, `Comma`,
`Period`, `Digit0`-`9`, `KeyA`-`Z`, `Enter`, `Escape`, `Backspace`, `Delete`, `Tab`, `F5`); a keydown matches when `e.key`
or `e.code` matches, so the macOS Korean layout (`` ` `` and `\` type `₩`) still triggers `` Ctrl+` `` and `Mod+\`. Events
with `e.isComposing` or `keyCode === 229` are ignored except Escape. `ctrl: true` requires `e.ctrlKey` and, off macOS,
also satisfies `mod`; on macOS a binding without `ctrl` requires `!e.ctrlKey`; `shift` / `alt` match exactly. KeyHint
labels stay as written (`⌘\`, `` Ctrl+` ``, `⌘⌫` / `Delete`).

**macOS menu bar (core, `src-tauri`).** A custom menu replaces Tauri's default, whose Close Window ⌘W would close the app
instead of a tab. App: About BulkDB → `app.about`, Settings… ⌘, → `app.settings`, predefined Services / Hide / Hide Others
/ Quit. File: New Connection ⌘N, New Window ⇧⌘N (`window.new`), New BulkDB Instance (`app.newInstance`), New Query ⌘T, Open SQL File… ⌘O, Copy Database… (`copy.open`), Close Tab ⌘W → `tabs.close`, Close Window ⇧⌘W (custom
item closing the window in Rust; the predefined one hard-codes ⌘W). Edit: predefined Undo, Redo, Cut, Copy, Paste, Select
All (mandatory: WKWebView clipboard keys need them). View: Toggle Sidebar ⌘0, Toggle Inspector ⌘\, Console ⌘J, Refresh
⌘R, Command Palette ⌘P. Query: Run ⌘↵, Explain ⌘E, Check ⇧⌘↵, Stop ⌘. (`jobs.cancelAsk`). Window: predefined Minimize,
Zoom. Each custom item emits `menu:action` with its action id; AppEvents calls `runAction(id)` (disabled = no-op). KEYMAP
rows with `menuOwned: true` are skipped by the JS dispatcher only when `inTauri() && isMac()`, so nothing fires twice.
Windows has no menu bar; the JS dispatcher handles everything.

CodeMirror internals unchanged (`Ctrl+Space` completion, `Mod+/` comment, undo). In mock/browser mode `Mod+T/W/N`, `Mod+Shift+N` and
`Ctrl+Tab` belong to the browser; palette and buttons cover them. Labels render through `KeyHint` (`⌘⇧↩` on macOS,
`Ctrl+Shift+Enter` on Windows).

---

## 7. Contracts (ui-foundation implements; everyone codes against them)

### 7.1 Stores

**Dependency order.** Leaves: `ui`, `settings`, `logs`; then `connections` → `jobs` → `catalog` → `extract` → `tabs` →
`workspace`; `lib/*-actions` and `lib/actions` on top. A store imports only stores before it; nothing in `stores/` or
`lib/` imports the `@/stores` barrel. Every selector returning an array or object (`selectTabList`, `selectJobsForConn`,
history pages, `useQueryDoc` with an object) is used with `useShallow`; prefer selecting primitives. **Test resets:**
`src/test/stores.ts` resets every store (tabs `nextId` and workspace included); `lib/actions`, `lib/toast` and
`lib/confirm` export `__resetForTests()`, called in `src/test/setup.ts` `afterEach`.

**`src/stores/tabs.ts` (new)**

```ts
import type { TableRef } from 'ipc-types';  import type { QueryDoc, QueryDocInit } from './extract';
export type TabKind = 'query' | 'table' | 'history';  export type TableView = 'structure' | 'query';
export type StructureSegment = 'columns' | 'indexes' | 'ddl' | 'overview';
interface TabBase { id: string; connId: string; customTitle?: string }
export interface QueryTab extends TabBase { kind: 'query'; seq: number }          // "쿼리 {seq}"
export interface TableTab extends TabBase { kind: 'table'; table: TableRef; preview: boolean; view: TableView; segment: StructureSegment }
export interface HistoryTab extends TabBase { kind: 'history' }                    // one per workspace
export type WorkspaceTab = QueryTab | TableTab | HistoryTab;
export interface ClosedTab { tab: WorkspaceTab; doc?: QueryDoc; index: number }
export interface TabSet { ids: string[]; activeId: string | null; closed: ClosedTab[] } // closed ≤ 10
export interface TabsState {
  nextId: number; tabs: Record<string, WorkspaceTab>; sets: Record<string, TabSet>; // sets key: connId
  openQuery(connId: string, init?: QueryDocInit & { title?: string }): string;
  openTable(connId: string, table: TableRef,
    opts?: { preview?: boolean; view?: TableView; segment?: StructureSegment; newTab?: boolean }): string;
  openHistory(connId: string): string;
  close(id: string): void;  closeOthers(id: string): void;  closeToRight(id: string): void;  closeAll(connId: string): void;
  reopenClosed(connId: string): string | null;
  activate(id: string): void;  activateIndex(connId: string, index: number): void;  // 0-based, -1 = last
  activateNext(connId: string, delta: 1 | -1): void;  reorder(connId: string, from: number, to: number): void;
  rename(id: string, title: string | null): void;  pin(id: string): void;          // null/'' = default title
  setTableView(id: string, view: TableView): Promise<void>;  setSegment(id: string, segment: StructureSegment): void;
  dropConnection(connId: string): void;                                            // disposes tabs + docs
}
export const useTabsStore: UseBoundStore<StoreApi<TabsState>>;
export function selectTabList(s: TabsState, connId: string | null): WorkspaceTab[]; // with useShallow
export function selectActiveTab(s: TabsState, connId: string | null): WorkspaceTab | undefined;
export function selectActiveQueryTarget(s: TabsState, connId: string | null): string | null; // query tab or table tab in Query view
export function useActiveQueryTarget(): string | null;                             // reactive, active workspace
export function tabTitle(tab: WorkspaceTab, doc?: QueryDoc): string;
```

Ids `q{n}` / `t{n}` / `h{n}` come from `nextId`; a doc's id **is** its tab id; a new query tab's `seq` = highest open
`seq` in its set + 1. New tabs go after the active one and are activated. `openTable` without `newTab`: a tab for the same
`connId + database + schema + name` is activated (pinned unless `preview`); else with `preview` the set's preview tab is
**replaced by a new tab** (new id, same index, not pushed to `closed`); else a new tab. `newTab: true` always creates a new
pinned tab. Preview tabs never have docs: Query view, double-click and rename pin. `setTableView(id, 'query')`: refused
when `!canTargetTable`; pins; without a doc, `createDoc(id, connId, { name: table.name })` with `busy: 'select'`, then
`catalog_select_sql` → patch `{ sql, baseline: sql, busy: null }` or `{ error, busy: null }` (the view stays 쿼리); the doc
survives toggling back to 구조 and is disposed with the tab. `close` leaves the job running, pushes `{ tab, doc: { ...doc,
busy: null }, index }` onto `closed`, then `disposeDoc`; never asks; `reopenClosed` restores both. Not persisted.

**`src/stores/workspace.ts` (new)**

```ts
import type { HistoryEntry, TableRef } from 'ipc-types';
export type AppMode = 'welcome' | 'workspace';
export interface BrowseScope { database: string | null; schema: string | null }
export interface WorkspaceState {
  mode: AppMode; openConnId: string | null; opened: string[];     // opened: in-memory workspaces, MRU first
  scope: Record<string, BrowseScope>; selection: Record<string, TableRef | null>; // per connection
  openWorkspace(connId: string): Promise<boolean>;
  showWelcome(): void;                                            // keeps workspaces in memory
  closeWorkspace(connId: string, opts?: { force?: boolean }): Promise<boolean>;
  ensureScope(connId: string): Promise<void>;  setScope(connId: string, patch: Partial<BrowseScope>): void;
  select(connId: string, ref: TableRef | null): void;
  openFromHistory(entry: HistoryEntry, opts?: { run?: boolean }): Promise<string | null>;
}
export const useWorkspaceStore: UseBoundStore<StoreApi<WorkspaceState>>;
```

- `openWorkspace(id)`: unknown id → toast "연결을 찾을 수 없습니다", `false`. `needsSessionPassword` →
  `ui.askDbPassword(id, 'open')`, `false`. Otherwise **synchronously before any await**: `mode = 'workspace'`, `openConnId
  = id`, `connections.select(id)`, MRU; then `await settings.ensureLoaded()` (resolves on failure too) and one query tab if
  the set is empty (after the await only per-connection state is written, so clicking A then B lands on B). Fire and
  forget: `catalog.loadDatabases(id)` → `ensureScope(id)`; `connections.test(id)` unless `ok` (fills the pill version;
  failure = pill error, workspace stays). Resolves `true` without waiting.
- `ensureScope`: keeps a valid existing scope; database = `conn.database` if listed and accessible, else `isCurrent`, else
  the first accessible; then `loadSchemas` and the schema rule: the schema equal to `conn.user` (case-insensitive), else
  `public` (postgres) / `dbo` (mssql) / `main` (sqlite) if present, else the first. `setScope({ database })` clears the
  schema and re-runs the schema rule.
- `closeWorkspace`: `askConfirm` if a doc is dirty unless `force`; `tabs.dropConnection`; drops scope and selection;
  closing the open workspace goes to Welcome. `deleteConnection` passes `force: true` (a single confirm).
- `openFromHistory`: unknown connection → toast, `null`; password needed → prompt with a `then` that does the rest, returns
  `null`; else opens the entry's workspace (switching workspaces for another connection), `tabs.openQuery(entry.connId, {
  sql, sqlMode, sqlFile, name: entry.label, session, output })`, `run(tabId)` if `run`; returns the tab id.

**`src/stores/extract.ts` (reworked into per-tab documents)**

```ts
export type SqlMode = 'text' | 'file';  export type ExportPanelSection = 'output' | 'session' | 'plan' | 'check' | 'job';
export type ExtractBusy = 'check' | 'preflight' | 'starting' | 'explain' | 'file' | 'select' | null;
export interface QueryDoc {
  tabId: string; connId: string; gen: number;        // connId fixed at creation; gen bumped by createDoc / restoreDoc
  sqlMode: SqlMode; sql: string; baseline: string;   // dirty = sql !== baseline
  sqlFile: string; sqlFileInfo: SqlFile | null; name: string; session: SessionOpts; output: OutputOpts;
  lastPreflight: PreflightReport | null; lastPlan: PlanSummary | null; lastCheck: SqlCheck | null;
  busy: ExtractBusy; error: string | null; lastJobId: string | null;
  panel: ExportPanelSection; panelCollapsed: boolean; // new docs: 'output', collapsed
}
export interface QueryDocInit { sql?: string; sqlMode?: SqlMode; sqlFile?: string; name?: string; session?: SessionOpts; output?: Partial<OutputOpts> }
export type PreflightDialogState =
  | { kind: 'run'; tabId: string; req: ExportRequest; report: PreflightReport; busy: boolean; error: string | null }
  | { kind: 'resume'; jobId: string; report: PreflightReport; busy: boolean; error: string | null };
export interface ExtractState {
  docs: Record<string, QueryDoc>;
  preflightDialog: PreflightDialogState | null;      // the open sheet
  preflightQueue: PreflightDialogState[];            // reports that arrived while a sheet was open
  createDoc(tabId: string, connId: string, init?: QueryDocInit): QueryDoc;
  disposeDoc(tabId: string): void;  restoreDoc(doc: QueryDoc): void;   // restore: busy null, new gen
  // setters, all (tabId, …) with today's shapes: setSqlMode, setSql, setSqlFile, setName, setSession, setOutput, setCsv,
  // setParquet, setSplit, applyExcelPreset, setPanel (also expands), togglePanel, clearError
  buildRequest(tabId: string): ExportRequest | null;
  check(tabId: string): Promise<SqlCheck | null>;  checkSql(tabId: string, sql: string): Promise<SqlCheck | null>;
  run(tabId: string): Promise<void>;  explain(tabId: string, analyze?: boolean): Promise<void>;
  loadSqlFile(tabId: string, path?: string): Promise<void>;  startAcknowledged(acks: string[]): Promise<string | null>;
  openResumeDialog(jobId: string, report: PreflightReport): void;  closeDialog(): void; // then the next queued sheet
}
export function useQueryDoc<T>(tabId: string, select: (d: QueryDoc) => T): T | undefined;
export function isDocDirty(d: QueryDoc): boolean;
// kept: DEFAULT_NAME_PATTERN, EXCEL_SPLIT_ROWS, defaultCsvOpts, defaultParquetOpts, defaultOutputOpts
```

Removed: the flat single-document fields, `effectiveConnId`, `setConnId`, `applyConnectionDefaults`, `loadFromHistory`.
`createDoc` reads connection defaults and settings **at call time** (session = `conn.session`; dir = `conn.outputDir` →
`settings.defaultOutputDir` → ''; CSV CRLF = `settings.csvCrlfDefault`). **Async attachment rule:** every action takes
`tabId`, reads the doc and its `gen` once at the start, and writes only through an internal `patch(tabId, gen, partial)`
that is a no-op when the doc is gone or `gen` differs (a closed and reopened tab never gets a stale result); no action
reads "the active tab" after an `await`. A check or plan is kept only if the doc's text (mode, file) still equals what was
sent. `run` stores `lastPreflight`; if the doc is gone when preflight returns, it neither starts nor opens a sheet
(`logLocal('info')` only); with warnings it opens `{ kind: 'run', tabId, req }`, or queues it while another sheet (run or
resume) is open; else it starts. On `export_start` success it patches `{ lastJobId, panel: 'job', panelCollapsed: false,
baseline: sql }`. `startAcknowledged` uses the captured `req` (the user confirmed, so the job starts even if the tab
closed); its errors and busy flag live in `preflightDialog`; success or `closeDialog` opens the next queued sheet.
`baseline` changes only at `createDoc`, `loadSqlFile` and a successful `export_start`. Passwords: `run` / `explain` call
`ui.askDbPassword(connId, 'run' | 'explain', { tabId })`.

**`src/stores/ui.ts`.** Removed: `selectedTable`, `selectTable`, `centerTab`, `setCenterTab`, `CenterTab`, `SelectedTable`,
`dbPasswordPrompt` (→ queue), `logCollapsed`, `showActivity` (→ `lib/job-actions`; it needs `workspace.mode`). Added:

```ts
export type SidebarSegment = 'items' | 'history';  export type InspectorSegment = 'activity' | 'details';
export type WelcomeGroupBy = 'none' | 'tag' | 'driver';  export type PalettePick = { kind: 'openInConnection'; tabId: string };
export type PaletteScope = 'all' | 'connections' | 'tables' | 'history' | 'actions';
export interface SidebarRequest { kind: 'focusSearch' | 'database' | 'schema'; nonce: number }
export type DbPasswordReason = 'open' | 'browse' | 'test' | 'run' | 'explain';
export interface DbPasswordRequest { reason: DbPasswordReason; tabId?: string; then?: () => void }
export interface DbPasswordPrompt { id: number; connId: string; requests: DbPasswordRequest[] }
// state
sidebarOpen: boolean; sidebarSegment: SidebarSegment; inspectorOpen: boolean /* default false */;
inspectorSegment: InspectorSegment; exportPanelHeight: number; welcomeGroupBy: WelcomeGroupBy;
collapsedSections: Record<string, boolean>; sidebarRequest: SidebarRequest | null; dbPasswordPrompts: DbPasswordPrompt[];
palette: { open: boolean; scope: PaletteScope; query: string; pick?: PalettePick };
aboutOpen: boolean; importDbeaverOpen: boolean; activityPopoverOpen: boolean;
// actions
setSidebarOpen(o: boolean): void; toggleSidebar(): void; setSidebarSegment(s: SidebarSegment): void;
setInspectorOpen(o: boolean): void; toggleInspector(): void; setInspectorSegment(s: InspectorSegment): void;
requestSidebar(kind: SidebarRequest['kind']): void;  // also sidebarOpen = true, sidebarSegment = 'items'
clearSidebarRequest(): void; setActivityPopoverOpen(o: boolean): void; setExportPanelHeight(px: number): void;
setWelcomeGroupBy(g: WelcomeGroupBy): void; toggleSection(key: string): void; setAboutOpen(o: boolean): void;
openPalette(scope?: PaletteScope, query?: string, pick?: PalettePick): void; closePalette(): void;
setImportDbeaverOpen(o: boolean): void; resolveDbPassword(id: number): void;  // removes that prompt
askDbPassword(connId: string, reason: DbPasswordReason, opts?: { tabId?: string; then?: () => void }): void;
```

`askDbPassword` for a connId that already has a queued prompt appends a request to it (nothing is overwritten); otherwise
it queues a new prompt (oldest first, like `hostKeyPrompts`). DbPasswordDialog shows the head; on success it runs each
request in order, awaiting it: `open` → `if (await openWorkspace(connId)) then?.()`; `browse` → `await
catalog.loadDatabases(connId, true)` + `ensureScope`; `run` / `explain` → `extract.run(tabId)` / `explain(tabId)` when
`tabId` is set; `test` → nothing; non-`open` requests then call `then?.()`. Kept: `theme`, `lang`, `leftWidth` (=
sidebar), `rightWidth` (= inspector), `logHeight`, `logOpen`, `toggleLog`, `connectionDialog`, `settingsOpen`,
`hostKeyPrompts`, `sshPasswordPrompts`, `cancelConfirmJobId`. `LAYOUT_LIMITS` and defaults follow section 4. Persisted
(persist `version: 1`, `migrate` clamps old values): `sidebarOpen`, `inspectorOpen`, `sidebarSegment`, `inspectorSegment`,
`leftWidth`, `rightWidth`, `logHeight`, `exportPanelHeight`, `welcomeGroupBy`, `collapsedSections`.

**`src/stores/history.ts`** keeps server-side filtering: `lists: Record<string, HistoryPage>` (key: connId, or `'*'` for
all; `HistoryPage = { entries: HistoryEntry[]; loading: boolean; error: string | null }`), `load(connId: string | null,
limit = 500)` → `history_list(connId ?? undefined, limit)`, `reloadLoaded()` (every loaded key; AppEvents calls it after
an export). Client-side search covers the loaded page only. The flat `connId` / `entries` / `setConnId` stay until W3.

**Other stores** keep their APIs. Additions: jobs `selectJobsForConn(s, connId)`, `selectPrimaryActiveJob(s)` (warned >
running > finishing > preflight > pending); settings `ensureLoaded(): Promise<void>`. `catalog.expanded` stays.

### 7.2 Connection environment tag (core → ipc-types)

Rust (`crates/core/src/types.rs`): `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")] pub enum ConnTag { Local, Development, Testing, Staging, Production }`, and on
`Connection`: `#[serde(default, skip_serializing_if = "Option::is_none")] #[ts(optional)] pub tag: Option<ConnTag>,`
(reaches `ConnectionInput` through `flatten`). Generated: `export type ConnTag = "local" | "development" | "testing" |
"staging" | "production";`, `Connection.tag?: ConnTag`. CLI: `conn add --tag`, `conn edit --tag` / `--clear-tag`, `conn
list` / `show` print it. Optional: DBeaver import maps connection type `dev` / `test` / `prod`. Then `pnpm typegen`;
configs without `tag` load unchanged. Mock (`lib/mock-ipc.ts`, foundation): `conn_save` copies `tag: input.tag` into the
returned `Connection` (omitted = cleared); mock connections get tags (prod → production, local → local, one staging).
UI helpers in `src/lib/env.ts`:

```ts
export type EnvTag = ConnTag;                    // re-export from ipc-types
export const ENV_TAGS: readonly EnvTag[];        // ['local','development','testing','staging','production']
export function connTag(conn: Connection | undefined): EnvTag | undefined;
export function envLabel(tag: EnvTag): string;   // 로컬 / 개발 / 테스트 / 스테이징 / 운영 (en: Local …)
```

The tag key (`production`; `PRODUCTION` in the pill) stays English in both languages; pickers show `production — 운영`.
Until typegen lands, `env.ts` declares the same union locally (TODO) and then becomes the re-export.

### 7.3 i18n: per-folder strings

```ts
// src/lib/i18n.ts (added; t, getLang, setLang, stateLabel, phaseLabel stay)
export type Translator<K extends string> = {
  (key: K, params?: Record<string, string | number>): string; readonly ns: string; readonly keys: readonly K[];
};
export function defineStrings<const E extends Record<string, string>>(
  ns: string, tables: { en: E; ko: { readonly [K in keyof E]: string } },
): Translator<Extract<keyof E, string>>;
export function listNamespaces(): readonly string[];   // tests assert uniqueness
```

Each feature folder has `<folder>/strings.ts`: `export const ts = defineStrings('sidebar', { en: {…}, ko: {…} })`. Lookup
at call time (a language switch re-renders via AppShell `key={lang}`; AppEvents sits outside that subtree, so it never
re-subscribes or re-bootstraps), fallback `en` then the key, `{name}` placeholders like `t()`. `defineStrings` tolerates
re-registration (HMR replaces the namespace); uniqueness is a test, not a throw. Owners never add keys to `i18n.ts`; they
move every non-shared string they use (`tree.*`, `table.*`, `extract.*`, `history.*`, `monitor.*`, `log.*`) into their own
`strings.ts`; foundation deletes the legacy keys in W3. Shared keys (`common.*`, `state.*`, `phase.*`, `severity.*`,
`estimated`, `rows`) remain. Glossary: 연결 Connection · 연결 (button) Connect · 작업공간 Workspace · 시작 화면 Welcome · 쿼리
N Query N · 실행 Run · 중지 Stop · 계획 보기 Explain · 검사 Check · 출력 Output · 세션 Session · 계획 Plan · 작업 Job · 추정
estimated · 정확한 행 수 Exact rows · 정확히 세기 Count exactly · 구체화된 뷰 Materialized views · 외부 테이블 Foreign tables ·
항목 Items · 기록 History · 활동 Activity · 세부 정보 Details · 개요 Overview · BulkDB 정보 About BulkDB · 실행 전 점검
Preflight · 차단 Blockers · 확인했습니다 I understand · 탐색할 데이터베이스 선택 Browse database · 탐색 전용 Browse only · 추출
쿼리 만들기 Make extraction query · 열 목록 복사 Copy column list · 다른 연결에서 열기 Open in another connection.

### 7.4 UI primitives (`src/components/ui/**`, exported from `ui/index.ts`)

```ts
Toolbar(p: { left?: ReactNode; center?: ReactNode; right?: ReactNode; className?: string })  // grid 1fr auto 1fr
ToolbarButton(p: { icon: ReactNode; label: string; shortcut?: string; onClick?: () => void; active?: boolean;
  disabled?: boolean; loading?: boolean; showLabel?: boolean; menu?: MenuItem[] })  // 28px ghost; tooltip "label (KeyHint)"
StatusPill(p: { tag?: EnvTag; name: string; detail: ReactNode; state: 'idle' | 'connecting' | 'ok' | 'error';
  progress?: number | 'indeterminate' | null; onClick?: () => void })
SegmentedControl<V extends string>(p: { value: V; onChange: (v: V) => void; onActivate?: (v: V) => void;
  variant?: 'track' | 'tabs'; size?: 'sm' | 'md'; 'aria-label': string; className?: string;
  options: { value: V; label: ReactNode; icon?: ReactNode; badge?: ReactNode; disabled?: boolean; title?: string;
    separatorBefore?: boolean }[] })
  // track (default): role=radiogroup; --bg-control track, 2px inset, radius 6, height 22 (sm) / 26 (md); active thumb
  //   --seg-thumb + --seg-thumb-shadow, radius 5, --fg 500; inactive --fg-secondary; never an accent fill.
  // tabs: role=tablist, no track, 12px labels, active --fg 500 + 2px --fg underline, inactive --fg-secondary.
  // onActivate fires on every click / Enter / Space, the active option included. Arrow keys move.
SearchField(p: { value: string; onChange: (v: string) => void; placeholder?: string; shortcut?: string; autoFocus?: boolean;
  inputRef?: Ref<HTMLInputElement>; onKeyDown?: KeyboardEventHandler<HTMLInputElement>; 'aria-label': string })
  // Esc with text: clears + preventDefault; empty: Esc passes through
SidebarSection(p: { id: string; title: ReactNode; count?: number; caption?: ReactNode; collapsed: boolean;
  onToggle: () => void; actions?: ReactNode; children: ReactNode })        // 24px sticky header, 12px/600 secondary
SidebarItem(p: { id: string; icon?: ReactNode; label: ReactNode; trailing?: ReactNode; selected?: boolean; preview?: boolean;
  muted?: boolean; depth?: number; title?: string; onClick?: () => void; onDoubleClick?: () => void;
  contextItems?: MenuItem[]; 'aria-label'?: string })  // 26px, role=option + aria-selected in a listbox; outline none
TabBar(p: { children: ReactNode; onNewTab: () => void; newTabContextItems?: MenuItem[]; onEmptyDoubleClick?: () => void;
  onReorder?: (from: number, to: number) => void; 'aria-label': string })  // role=tablist, 30px, equal-width tabs
TabItem(p: { id: string; title: string; icon?: ReactNode; active: boolean; dirty?: boolean; busy?: boolean; preview?: boolean;
  onActivate: () => void; onClose: () => void; onDoubleClick?: () => void; contextItems?: MenuItem[];
  renaming?: boolean; onRename?: (title: string | null) => void })        // role=tab; middle-click = onClose
InspectorPanel(p: { segment: string; onSegmentChange: (s: string) => void;
  segments: { value: string; label: ReactNode; badge?: ReactNode }[]; children: ReactNode })  // tabs variant, 28px
Popover(p: { open: boolean; onOpenChange: (o: boolean) => void; anchor: RefObject<HTMLElement | null>;
  align?: 'start' | 'center' | 'end'; side?: 'top' | 'bottom'; width?: number; initialFocus?: boolean;
  children: ReactNode; 'aria-label': string })
  // hand-built (no @radix-ui/react-popover): portal, role=dialog aria-modal=false; closes on outside pointerdown (not
  // inside [data-radix-popper-content-wrapper] or portals it opened), on Esc while focus is inside (consumed), on
  // anchor unmount; initialFocus (default true) moves focus in and back to the anchor on close
KeyHint(p: { combo: string; className?: string })     // 'mod+shift+enter', 'ctrl+`', 'f5', 'mod+backspace'
EnvTag(p: { tag: EnvTag | undefined; variant: 'dot' | 'text' | 'chip' | 'badge'; className?: string }) // chip/badge: -fill + white
TagPicker(p: { value: EnvTag | undefined; onChange: (t: EnvTag | undefined) => void; id?: string })
Sheet(p: { open: boolean; onOpenChange: (o: boolean) => void; title: ReactNode; description?: ReactNode; children: ReactNode;
  footer?: ReactNode; footerStart?: ReactNode; width?: 'sm' | 'md' | 'lg' | 'xl'; dismissible?: boolean })
  // Radix Dialog; top var(--sheet-top); Esc / scrim → onOpenChange(false), which callers map to the cancel answer
StateChip(p: { state: JobState })                     // tone + icon from lib/job-state; spinner on busy states
ProgressBar(p: { value?: number; indeterminate?: boolean; tone?: 'accent' | 'warning' | 'danger'; size?: 'xs' | 'sm' })
RowEstimate(p: { rows?: bigint | number; exact?: ExactCount; variant: 'compact' | 'full'; showLabel?: boolean })
  // compact '≈48.2M' (+ ' 추정' with showLabel) | exact '48,213,004 · 10-27'; full '≈ 48,213,000 (추정)';
  // tooltip + aria always say 추정 / 정확 (+ time)
StatusDot(p: { status: ConnStatus; title?: string })  // moved from layout/HeaderBar (which re-exports it until W3)
CodeBlock(p: { code: string; maxHeight?: number; 'aria-label': string })  // mono 12px <pre>, data-selectable
CopyButton(p: { getText: () => string | Promise<string>; label: string; size?: 'sm' | 'md' })  // icon → Check 1.2s
useListNavigation(o: { count: number; activeIndex: number; onMove: (i: number) => void; onOpen?: (i: number) => void;
  onDelete?: (i: number) => void }): { onKeyDown: KeyboardEventHandler; getItemProps(i: number): object }
  // ↑↓ Home End Enter (+ Delete / Mod+Backspace with onDelete), roving selection, aria-activedescendant
Highlight(p: { text: string; indices: readonly number[] });  ConfirmHost();  Toaster()   // hosts mounted by AppShell
```

Existing primitives: `Button` variants `primary` (accent-fill), `secondary` (raised: `--btn-bg` + `--btn-shadow`, hover
`--btn-bg-hover`, pressed `--btn-bg-pressed`), `ghost`, `danger` (text; filled only in confirms), `link`; sizes sm 22 / md
26, radius 6; `outline` = alias of `secondary`. `IconButton` 22 / 26, radius 6, hover `bg-hover`. `Input` / `Select` /
`NumberInput` 26px (`dense` 22px), `bg-input`, 1px `--border-input`, radius 6, focus per 3.3. `Badge`: soft tints, no
border, 18px high, 11px Latin / 12px Hangul. `Notice`: tinted, no border, radius 6. `Table`: 26px header (`bg-panel`, 12px
secondary) and rows, hairlines, no zebra. `Tabs` (Radix) is re-skinned like the track variant and kept; its item type is
renamed `TabsItem` (only user: `CenterTabs.tsx`, deleted), so `ui/index.ts` exports `{ Tabs, type TabsItem }` next to the
`TabItem` component. `Dialog`: 5.I chrome + `showClose?: boolean` (default false). `ContextMenu` / `DropdownMenu`:
`bg-elevated`, radius 8, 24px items, no hover transition; `MenuItem` gains `shortcut?: string` (KeyHint), `checked?:
boolean` and `kind?: 'item' | 'radio' | 'label'`. `SplitPane`: 1px separator, 6px hit area. `Kbd` renders like `KeyHint`.
`EmptyState`: 13px secondary + 12px hint.

### 7.5 Shared lib helpers (`src/lib/**`)

```ts
// lib/fuzzy.ts
export interface FuzzyMatch { score: number; indices: number[] }
export function fuzzyMatch(query: string, text: string, mode?: 'fuzzy' | 'contains'): FuzzyMatch | null; // ^ $ anchors
export function fuzzyFilter<T>(items: readonly T[], query: string, key: (t: T) => string, limit?: number): { item: T; match: FuzzyMatch }[];
// lib/actions.ts
export type ActionGroup = 'query' | 'tabs' | 'view' | 'catalog' | 'connection' | 'jobs' | 'app';
export interface AppAction { id: string; group: ActionGroup; title: () => string; keywords?: string[];
  shortcut?: string; enabled?: () => boolean; run: () => void | Promise<void> }
export function getActions(): AppAction[];
export function runAction(id: string): Promise<void>;   // skips disabled; catches: logLocal('error') + toast danger
export function registerActions(actions: AppAction[]): () => void;          // returns unregister
export function registerBuiltinActions(): void;         // idempotent; AppShell and palette tests call it
// lib/keymap.ts (section 6)
export interface KeyBinding { combo: string; code?: string; action: string;
  scope: 'always' | 'welcome' | 'workspace' | 'queryTarget'; inInputs?: boolean; menuOwned?: boolean }
export const KEYMAP: readonly KeyBinding[];  export function comboLabel(combo: string): string;
// lib/shortcuts.ts: ShortcutBinding gains `code?`, `ctrl?: boolean` (physical Ctrl), `alt?: boolean`; matching per 6
// lib/table-actions.ts
export function execDatabase(connId: string): string | null; // conn.database → probed ServerInfo.database → isCurrent db
export function canTargetTable(connId: string, ref: TableRef): { ok: true } | { ok: false; reason: string };
export function openExtractionQuery(connId: string, ref: TableRef): Promise<string | null>; // pinned table tab, Query view
export function copyColumnList(connId: string, ref: TableRef): Promise<boolean>;
export function copyTableName(ref: TableRef, qualified?: boolean): Promise<boolean>;
export function copyDdl(connId: string, ref: TableRef): Promise<boolean>;
export function countExactly(connId: string, ref: TableRef): Promise<string | null>; // confirm rule; today's race guard
// lib/connection-actions.ts
export function newConnection(): void;  export function editConnection(connId: string): void;
export function testConnection(connId: string): Promise<ProbeResult | null>;       // password prompt 'test'
export function deleteConnection(connId: string): Promise<boolean>;  // confirm, remove, clear catalog, closeWorkspace force
export function duplicateConnection(connId: string): Promise<Connection | null>;
export function afterConnectionSaved(connId: string): Promise<void>;
export function openQueryInConnection(tabId: string, targetConnId: string): Promise<string | null>;
export function rescanDrivers(): Promise<number | null>;                          // log + toast "N개 사용 가능"
export function startImportDbeaver(): void;                                       // ui.setImportDbeaverOpen(true)
// lib/job-actions.ts
export function showActivity(): void;  // workspace: inspector on 'activity' if closed; welcome: popover; no focus move
export function askCancel(jobId: string): void;              // sets ui.cancelConfirmJobId + showActivity()
export function askCancelLatest(): void;                     // Esc / Mod+. behaviour, section 6
export function cancelJob(jobId: string): Promise<void>;     // clears the confirm, logs failures
export function reviewWarnings(jobId: string): Promise<boolean>; // fetch report if missing, open the resume sheet
// lib/confirm.ts, lib/toast.ts
export function askConfirm(o: ConfirmOptions /* title, body?, confirmLabel?, cancelLabel?, danger? */): Promise<boolean>;
export function setConfirmHandler(fn: ((o: ConfirmOptions) => Promise<boolean>) | null): void; // tests; null = ConfirmHost
export function toast(message: string, o?: { tone?: 'neutral' | 'success' | 'danger'; durationMs?: number }): void;
```

- `canTargetTable`: postgres, oracle, sqlite and odbc qualify only `schema.table` (`select_all_sql` in
  `crates/core/src/catalog/mod.rs`), so a table whose `ref.database` is set and differs from `execDatabase` is refused
  ("탐색 전용 데이터베이스입니다. 쿼리는 {execDb}에서 실행됩니다"); mysql and mssql qualify the database and are allowed. It
  gates 추출 쿼리 만들기, `table.makeQuery`, `table.showQuery`, `openExtractionQuery` and the 쿼리 view.
- `countExactly`: when the catalog estimate is ≥ 10M rows or unknown, first `askConfirm` ("{name}의 전체 행을 셉니다(약
  2.1B행, 추정). 서버 부하가 클 수 있습니다."); declining returns `null`. A confirm before the IPC call, not job state.
- `duplicateConnection`: `conn_save` of a copy with `id: ''`, name `<name> 복사본` / `<name> copy`, no secrets, output-only
  `hasPassword` / `hasSshSecret` stripped, `savePassword: false` when the driver has a `password` field (the first use asks
  through DbPasswordDialog instead of failing with AUTH; SSH secrets are asked by the tunnel's prompt as usual).
- `afterConnectionSaved(id)`: if `workspace.opened.includes(id)`: `catalog.refresh(id)`, reset scope + `ensureScope(id)`,
  `connections.test(id)` (refreshes the pill); docs keep their SQL; toast "연결 설정이 바뀌어 메타데이터를 다시 불러옵니다".
- `openQueryInConnection(tabId, target)`: copies `{ sql, sqlMode, sqlFile, name, output }` from the doc,
  `openWorkspace(target)` (a password prompt continues via `then`), then `tabs.openQuery(target, init)`; the session comes
  from the target's defaults (as `setConnId` did).
- Copy helpers also `logLocal('info', …)`. Built-in action ids (registered by ui-foundation): `palette.open`,
  `palette.connections`, `tabs.newQuery`, `tabs.close`, `tabs.reopenClosed`, `tabs.openHistory`, `query.run`,
  `query.explain`, `query.check`, `query.openFile`, `query.excelPreset`, `query.openInConnection` (palette pick mode),
  `view.toggleSidebar`, `view.toggleInspector`, `view.toggleConsole`, `view.toggleExportPanel`, `catalog.refresh`,
  `catalog.switchDatabase`, `catalog.switchSchema`, `table.countExactly`, `table.copyColumns`, `table.copyDdl`,
  `table.makeQuery`, `table.showStructure`, `table.showQuery`, `connection.new`, `connection.edit`, `connection.test`,
  `connection.importDbeaver`, `drivers.rescan`, `app.welcome`, `app.closeWorkspace`, `app.settings`, `app.about`,
  `app.diagnostics` (opens About), `theme.system|light|dark`, `lang.ko|en`, `jobs.cancelAsk`, `jobs.clearFinished`,
  `log.clear`, `sidebar.focusSearch`. Table actions target the active table tab, else the sidebar selection.

### 7.6 Slots (foundation mounts; owners own the files)

| slot | file (`src/components/…`) | export and props | owner | mounted in |
|---|---|---|---|---|
| WelcomeScreen | `welcome/WelcomeScreen.tsx` | `WelcomeScreen({ headerEnd }: { headerEnd?: ReactNode })` | welcome | AppShell (welcome backdrop) |
| ConnectionDialog | `dialogs/ConnectionDialog.tsx` | `ConnectionDialog()` (reads `ui.connectionDialog`) | welcome | AppShell |
| ImportDbeaverFlow | `dialogs/ImportDbeaverDialog.tsx` | `ImportDbeaverFlow()` (reads `ui.importDbeaverOpen`) | welcome | AppShell |
| Sidebar | `sidebar/Sidebar.tsx` | `Sidebar({ connId }: { connId: string })` | sidebar | SidebarFrame (항목) |
| HistoryList | `history/HistoryList.tsx` | `HistoryList({ connId }: { connId: string })` | structure | SidebarFrame (기록) |
| QueryTabView | `query/QueryTabView.tsx` | `QueryTabView({ tabId }: { tabId: string })` | query | TabContent; TableTabFrame (쿼리) |
| PreflightSheet | `query/PreflightSheet.tsx` | `PreflightSheet()` (reads `extract.preflightDialog`) | query | AppShell |
| StructureTabView | `table/StructureTabView.tsx` | `StructureTabView({ tabId, connId, table, segment, onSegmentChange }: { tabId: string; connId: string; table: TableRef; segment: StructureSegment; onSegmentChange: (s: StructureSegment) => void })` | structure | TableTabFrame (구조) |
| TableInspector | `table/TableInspector.tsx` | `TableInspector({ connId, table, source }: { connId: string; table: TableRef \| null; source: 'tab' \| 'selection' })` | structure | InspectorFrame (세부 정보) |
| HistoryView | `history/HistoryView.tsx` | `HistoryView({ tabId, connId }: { tabId: string; connId: string })` | structure | TabContent (history) |
| ActivityPanel | `monitor/ActivityPanel.tsx` | `ActivityPanel({ variant }: { variant: 'inspector' \| 'popover' })` | activity | InspectorFrame (활동); ActivityPopover |
| JobChip | `monitor/JobChip.tsx` | `JobChip({ variant }: { variant: 'toolbar' \| 'welcome' })` | activity | WorkspaceToolbar; AppShell → WelcomeScreen `headerEnd` |
| ConsoleDrawer | `log/ConsoleDrawer.tsx` | `ConsoleDrawer({ onClose }: { onClose: () => void })` | activity | ConsoleFrame |
| LogStatusButton | `log/LogStatusButton.tsx` | `LogStatusButton()` | activity | WorkspaceToolbar; AppShell → `headerEnd` |
| CommandPalette | `palette/CommandPalette.tsx` | `CommandPalette()` (reads `ui.palette`) | activity | AppShell |
| SettingsDialog / AboutDialog | `dialogs/SettingsDialog.tsx`, `dialogs/AboutDialog.tsx` | `SettingsDialog()`, `AboutDialog()` (no props; reads `ui.aboutOpen`) | activity | AppShell |
| prompts | `dialogs/{HostKey,SshPassword,DbPassword}Dialog.tsx` | `X()` (unchanged) | activity | AppShell |
| AppEvents | `AppEvents.tsx` | `AppEvents()` (also `menu:action` → `runAction`) | activity | AppShell |
| CopyWizard | `copy/CopyWizard.tsx` | `CopyWizard()` (reads `copy.wizard`; docs/db-copy.md 7) | copy-ui | AppShell |
| CopyJobSummary | `copy/CopyJobSummary.tsx` | `CopyJobSummary({ summary }: { summary: JobSummary })` (kind `copy`) | copy-ui | JobRow |
| JobTitle | `monitor/JobTitle.tsx` | `JobTitle({ summary }: { summary: JobSummary })`; `jobTitle(summary)` for tooltips / labels. Copy jobs: `Copy` icon (JobRow) and "복사: {src}.{db} → {tgt}.{db}", truncated in the middle for the ~300px rows (the ends share a short line 2 : 3 in favour of the target, connection names take the ellipsis, both `.database` parts stay), full text as tooltip | activity | JobRow; JobChip tooltip |
| InstancesChip | `welcome/InstancesChip.tsx` | `InstancesChip({ className }: { className?: string })` ("다른 BulkDB {n}개 실행 중"; nothing when alone) | windows | WelcomeScreen header; StatusPillPopover |
| windows (no UI) | `stores/windows.ts` | `initWindows(): Promise<void>`: `window_init`, the title sync, the shared ui slice, the close question and the one quit question (`app:quit-requested`, never also in AppEvents), the instances poll | windows | AppShell (once) |

**Copy wizard look (CopyWizard, docs/db-copy.md 7).** One `SegmentedControl` only: the steps bar (원본·대상 / 개체 / 옵션 /
계획, then 진행 / 결과). Every other choice in the wizard is a native radio list (`copy/parts.tsx` `RadioList`; 전체 / 테이블
선택 as an `inline` row), so the two never look alike. Connection `Select`s render each option and the trigger as
`DriverBadge size="sm"` + name + `EnvTag chip` (+ "쓰기 허용 아님" / "복사 지원 안 함" in `fg-tertiary` when disabled), via
`SelectOption.content`; the endpoint caption under it has no second EnvTag. `Mod+Enter` inside the sheet = [복사 시작] on the
plan step, or [계속] for a warned job, only while that button is enabled (a dry run stays a click).

`TableInspector` with `source: 'selection'` shows only data already in the catalog store (TableInfo: estimate, sizes,
comment, kind) plus cached details and exact counts, with no IPC call; it calls `loadTableDetails` only for `source:
'tab'`. `SqlEditor` is query-internal (`query/SqlEditor.tsx`), not a slot, never stubbed.

**Slot stub exception.** So the foundation's imports resolve while owners work in parallel, the foundation may create a
slot file from this table that **does not exist yet**, containing only the export above rendering an `EmptyState` with the
slot name, first line `// SLOT STUB (ui-foundation). Owner: <key>. Replace this file entirely.` It never edits or
overwrites an existing file outside its ownership. An owner replacing a stub with a moved file runs `git rm` on the stub,
then `git mv`. Layout files never change for an owner's needs: request it in `notDone`.

### 7.7 Foundation as built (W1)

Everything in 7.1-7.6 exists. Supersets added in W1 (owners may use them; nothing above changed meaning):

- **Stores.** tabs: `workspaceConnId` + `setWorkspaceConn` (kept by the workspace store; `useActiveQueryTarget`
  reads it), `isQueryTarget(tab)`, `CLOSED_LIMIT`, `tabTitle(tab, doc?: Pick<QueryDoc, 'name'>)`. workspace:
  `openWorkspace(connId, opts?: { skipEmptyTab?: boolean })`, `pickSchema(schemas, user, driver)`. ui: `LAYOUT_DEFAULTS`,
  `migrateUi(persisted)`. history: `selectHistoryPage(s, connId)`, `historyKey`, `HISTORY_ALL`. catalog / settings /
  workspace: `__reset…ForTests()` (called by `resetStores()`; a mock reset leaves in-flight loads pending forever).
- **Lib.** `useExecDatabase(connId)` (reactive `execDatabase`, from `lib/table-actions` or `lib/table-target`);
  `resumeDbPasswordRequests(prompt)` (`lib/db-password`: the 7.1 resume rules for the DbPassword dialog);
  `jobProgressPercent(progress)` (`lib/job-state`, elapsed / (elapsed + ETA)); `formatClock(sec)` (`3:12`);
  `useIsMac()` / `useInTauri()` (hydration-safe: the static export prerenders `false`); keymap `shortcutFor(action)`,
  `parseCombo`, `bindingFor`, `comboLabel(combo, mac?)`; actions `getAction`, `isActionEnabled`, `AppAction.hidden`
  (not listed in the palette: `tabs.select1`-`8`, `tabs.selectLast`, `palette.open`); extra built-in ids `tabs.select1`-`8`,
  `tabs.selectLast`, `tabs.next`, `tabs.previous`; env `ENV_GROUP_ORDER`, `isEnvTag`, `noTagLabel`, `envOptionLabel`,
  `withConnTag(input, tag)`, `envVars(tag)`; shortcuts `when?` on a binding, `isEditable`, `editableHasText`, `inDialog`.
- **Primitives.** `SegmentedControl fill?`; `Toolbar dragRegion?`; `TabItem tooltip?`, `onRenameCancel?`; `TabBar
  empty?`; `SearchField size?`, `className?`; `KeyHint plain?`; `Badge hangul?`; `Button variant="dangerFilled"`
  (confirms only; `bg-danger-fill`); `Dialog footerStart?`; `Sheet flush?`; `ModalChrome`; `StatusBar` (an inner 28px
  bar for the sidebar bottom bar, the table tab bar, the query bar and the console header, not an app status bar;
  `surface?: 'auto' | 'panel' | 'inherit'`: `auto` = `bg-panel`, transparent inside a `data-surface="sidebar"`
  container, which SidebarFrame and InspectorPanel set, so the sidebar's bottom bars stay `bg-sidebar`); `Spinner label?`,
  `decorative?` (a `status` named `us('loading')`; `decorative` = `aria-hidden`, for a spinner beside its own text or
  inside a labelled / `aria-busy` control: Button, ToolbarButton, StatusPill, StateChip); `escapeIsLocal(e)` and
  the `data-esc-local` attribute (Esc stays with the control, Dialog / Sheet do not close); `useListNavigation(...)`
  also returns `listProps` and `itemId(i)`; `ContextMenu disabled?`; `DropdownMenu open?`, `onOpenChange?`, `side?`;
  `SplitPane maxFraction?` (export panel ≤ 50%); `Tooltip delay?`; `selectContentClass` / `selectItemClass`,
  `MenuRowContent`. Primitives with tooltips need a `TooltipProvider` in tests (AppShell provides one).
- **CSS.** `.input-focus` (input focus ring), `.list-scroll`, `.sel-muted` (secondary text that turns white-muted in a
  focused selected row), `.animate-enter`, `.animate-enter-up`, `.animate-fade`, `.animate-indeterminate`, `--ring-input`.
- **Tests.** `src/test/setup.ts` imports only dependency-free modules, so a test file's `vi.mock('@/lib/commands')`
  reaches the stores; keep it that way.

**Transitional seams (deleted in W3).** So the app keeps working end to end while W2 lands, W1 keeps the pre-redesign
panels running instead of breaking them:
- `stores/extract.ts`: the legacy flat API (flat fields, `setConnId`, `applyConnectionDefaults`, `loadFromHistory`,
  `effectiveConnId`, and arity-dispatched overloads of the per-tab setters / actions without `tabId`) edits the doc bound
  with `bindLegacyDoc(tabId)` (the QueryTabView stub binds the active query target), else a private `LEGACY_DOC_ID` doc.
  New code never calls a method without `tabId`.
- `stores/ui.ts`: `selectedTable`, `selectTable`, `centerTab`, `setCenterTab`, `dbPasswordPrompt` (head of the queue,
  enumerable `{ connId, reason }`), `closeDbPassword`, `DbPasswordPrompt.reason`; `stores/history.ts`: the flat page.
- `layout/useLegacyBridge.ts` (old code's `setCenterTab('extract')` shows the edited query tab), `layout/slots.tsx`
  (`AboutSlot` / `ImportDbeaverSlot` render `AboutDialog()` / `ImportDbeaverFlow()` as soon as those exports exist, an
  adapter over the old exports until then), `layout/HeaderBar.tsx` (unmounted; re-exports `StatusDot`).

**Slot stubs created in W1** (first line `// SLOT STUB (ui-foundation)…`; each renders the current component):
`welcome/WelcomeScreen.tsx` (a minimal connection list), `sidebar/Sidebar.tsx` (ConnectionTree; a picked table opens
the preview tab), `history/HistoryList.tsx` (EmptyState + 전체 기록), `history/HistoryView.tsx` (HistoryTab),
`query/QueryTabView.tsx` (ExtractTab on the bound doc), `query/PreflightSheet.tsx` (PreflightDialog),
`table/StructureTabView.tsx` (TableTab), `table/TableInspector.tsx` (catalog data only), `monitor/ActivityPanel.tsx`
(MonitorPanel), `monitor/JobChip.tsx`, `log/ConsoleDrawer.tsx` (LogPanel), `log/LogStatusButton.tsx`,
`palette/CommandPalette.tsx` (renders nothing).

---

## 8. Feature parity checklist

| # | capability (today) | where in the new UI | owner |
|---|---|---|---|
| 1 | list saved connections | Welcome list (grouping, search) | welcome |
| 2 | create connection (driver-driven form) | Welcome `+` / 새 연결 / `Mod+N` / menu / palette → two-step form | welcome |
| 3 | edit connection | Welcome menu, pill popover, palette | welcome, ui-foundation |
| 4 | delete connection with confirm | Welcome menu / `Delete` / `Mod+Backspace` → `deleteConnection` | welcome, ui-foundation |
| 5 | test connection (incl. ask-every-time password) | Welcome menu, form [테스트], pill popover, palette | welcome, ui-foundation |
| 6 | status dot unknown/testing/ok/error + error tooltip | Welcome row, pill (error icon + text) | welcome, ui-foundation |
| 7 | SSH config (host, port, user, key/password/agent, key file, passphrase, compression) + ssh badge; host-key prompt with the raw ssh message | form SSH section; Welcome row line 2; pill popover; HostKey sheet | welcome, ui-foundation, activity |
| 8 | keyring vs ask every time, "(unchanged)", secrets sent once and cleared, clear stored password / SSH secret | form | welcome |
| 9 | unavailable driver notice, install link (+ error), rescan | form notice; dimmed step-1 tiles | welcome |
| 10 | driver fields (sslmode, instance, encrypt, trust cert, service name, SQLite path, ODBC driver, connection string) | form | welcome |
| 11 | per-connection default output folder; test result (driver, version, ms, tunnel, work_mem) | form | welcome |
| 12 | DBeaver import (pick, file path, parse error, per-row selection default ok + driverMissing, saved rows locked, retry failures only, password note, import N, failures) | Welcome, list menu, palette → ImportDbeaverFlow | welcome |
| 13 | rescan drivers with count | Welcome 정렬 / list menu, form, palette (`rescanDrivers`) | welcome, ui-foundation |
| 14 | databases (size, encoding/collation, current, inaccessible); schemas with table counts | sidebar database / schema popovers | sidebar |
| 15 | tables by kind with icons; estimate (labelled) + size + comment | sidebar sections (sticky 추정 captions), rows, tooltip | sidebar |
| 16 | lazy loads, spinners, errors + retry, empty states | sidebar | sidebar |
| 17 | ask-every-time password before browsing | `openWorkspace` (`open`); sidebar (`browse`) | ui-foundation, sidebar |
| 18 | refresh catalog (invalidate backend cache) | toolbar, sidebar bar, `Mod+R` / `F5`, menu, palette | ui-foundation, sidebar |
| 19 | make full extraction query (select-all SQL) | sidebar menu, structure header, palette → table tab 쿼리 (`canTargetTable`) | sidebar, structure, ui-foundation |
| 20 | copy column list | sidebar menu, 열 segment, palette | sidebar, structure, ui-foundation |
| 21 | columns (pos, name, PK, type, nullable, default, PK/UQ/FK + target, comment) | Structure 열 | structure |
| 22 | indexes (name, columns, kind, unique/PK, predicate, include, size, scans) | Structure 인덱스 | structure |
| 23 | DDL + copy DDL; refresh table details | Structure DDL (`CodeBlock`), header, palette | structure |
| 24 | estimate + sizes (total/table/index); kind, database | Structure 개요 + header, inspector, table tab bottom bar | structure, ui-foundation |
| 25 | exact count job (spinner, finished-before-id guard) | 개요, header, sidebar menu, inspector, palette → `countExactly` (+ ≥ 10M confirm) | structure, sidebar, ui-foundation |
| 26 | stored last exact count with time + duration | Structure 개요, inspector, sidebar `· 10-27` | structure, sidebar |
| 27 | SQL editor (dialect highlight, schema completion, dark theme) | query tab (`--syn-*` theme) | query |
| 28 | SQL file mode (path, browse, load, size/modified, read-only) | query bar `SQL ▾` menu, `Mod+O` | query, ui-foundation |
| 29 | name / label for `{name}`; target connection visible | 출력 first row; pill (name + execDb) | query, ui-foundation |
| 30 | run: preflight → start or sheet; `Mod+Enter` | query bar 실행, shortcut, menu, store | query, ui-foundation |
| 31 | explain: summary, plan tree, warnings + locate, raw plan; `Mod+E` | query bar 계획, panel 계획 | ui-foundation, query |
| 32 | SQL check: OK + kind / issues with codes | query bar 검사, `Mod+Shift+Enter`, panel 검사 | ui-foundation, query |
| 33 | dismissable error; busy states (checking, explaining, starting, loading file) | query strip, bar caption, run group, tab spinner | query, ui-foundation |
| 34 | session options (5 fields, per-driver availability) | panel 세션 | query |
| 35 | output options (folder, pattern, format, CSV opts, compression, Parquet opts, split rows/MB, overwrite) | panel 출력 | query |
| 36 | Excel preset | panel 출력, palette | query, ui-foundation |
| 37 | connection / settings defaults into the form | `createDoc` (+ `settings.ensureLoaded`) | ui-foundation |
| 38 | preflight: blockers, ack each warning, estimates, sessions, elapsed, start/continue, modal while starting, error | PreflightSheet (+ queue) | query |
| 39 | resume a warned job (fetch report if missing) | job row / panel 작업 [검토] → `reviewWarnings` → sheet (follows job state) | activity, ui-foundation, query |
| 40 | active jobs, 9 states, tones, spinner; progress, phase, elapsed, ETA, rows, rates (MB/s + rows/s), bytes, files | ActivityPanel, JobChip, pill, panel 작업 | activity, query, ui-foundation |
| 41 | server state, wait, bottleneck, temp bytes, pid | job row (inspector) | activity |
| 42 | cancel with confirm; `Esc` asks, 2nd `Esc` dismisses; confirm cleared on end | job row, query bar 중지, `Esc` / `Mod+.` → `askCancelLatest` | activity, query, ui-foundation |
| 43 | terminal results (error code/message/detail, exact count, rows/bytes/duration/verified, files + open folder, notes) | job row, panel 작업 | activity, query |
| 44 | remove card, clear finished, last 5 results; active job count | ActivityPanel; inspector badge, JobChip `+N` | activity, ui-foundation |
| 45 | jobs re-attach after restart; exact count to catalog; history reload after export | AppEvents (`history.reloadLoaded`) | activity |
| 46 | history (started, connection, label + SQL, state, rows, size, duration, files/error) + server-side filter by any saved connection | HistoryList (이 연결 / 전체), HistoryView (connection Select) | structure, ui-foundation |
| 47 | load an entry into the editor; run again; refresh | `openFromHistory`; buttons + menus; refresh | structure, ui-foundation |
| 48 | log rows (time, level colour, jobId, target, message), sticky scroll, level filter, clear | ConsoleDrawer, palette | activity |
| 49 | error count, flag until acked, auto-open on error; resizable persisted height | LogStatusButton (toolbar, Welcome header) + logs store; ConsoleFrame | activity, ui-foundation |
| 50 | settings (theme, language, jobs, log level, output dir, tunnel idle, intervals, CRLF, verify, diagnostics) | SettingsDialog via `Mod+,`, menu, palette, Welcome footer, pill popover | activity, ui-foundation |
| 51 | about (version, platform, arch, folders + open, drivers + tiers, copy info, diagnostics + reveal) | AboutDialog via palette, Welcome footer, Settings, macOS App menu | activity |
| 52 | theme and language switch (persisted) | Settings, palette | activity, ui-foundation |
| 53 | host-key (raw message; Esc = reject), SSH password, DB password prompts (queues, resume reasons, Esc = cancel answer) | prompt sheets (+ `open`, `tabId`, `then`, merged requests) | activity |
| 54 | mock badge; persisted panel sizes; shortcuts `Mod+Enter`, `Mod+E`, `Esc` | toolbar / Welcome `mock` chip; ui store; keymap | ui-foundation, welcome |
| 55 | bootstrap loads + events (job:*, host key, ssh password, drivers changed) | AppEvents | activity |
| 56 | open output folder | job row, panel 작업, history menu | activity, query, structure |
| 57 | connection tag (new) | form TagPicker, Welcome, pill, preflight | core, welcome, ui-foundation, query |
| 58 | re-target the same SQL + output options to another connection (header select + `setConnId`) | `query.openInConnection` 다른 연결에서 열기…: tab menu, `SQL ▾` menu, palette pick mode | ui-foundation, query, activity |
| 59 | several windows (new) | 새 창: `Mod+Shift+N`, File menu, palette, toolbar switcher; 새 창에서 열기: Welcome row menu / Welcome-local and palette-local `Mod+Enter`; "창 N" chips focusing a window: Welcome rows, palette connection rows; titles `BulkDB — <connection>` | windows, activity |
| 60 | several BulkDB processes (new) | 새 BulkDB 실행 (File menu, palette); InstancesChip (Welcome header, status pill); Activity footer note | windows, activity |
| 61 | close / quit with running jobs (new): one question, shared by Cmd+Q / the OS and closing the last window | `stores/windows.ts` (`app:quit-requested`, `onCloseRequested`) | windows |
| 62 | DB copy wizard (new): endpoints (badge + EnvTag selects, write-allowed marking, same-database refusal, production notice), objects (전체 / 테이블 선택 radios, tree, PG switches), options (skip default), plan (blockers + [제외], acknowledgements, per-table policy menu, SQL split, `Mod+Enter`), progress, result + resume, dry run, 최근 복사 | CopyWizard via Welcome ghost row and row menu, palette `copy.open` (workspace: its connection and browse database / schema preset), toolbar switcher menu, File menu | copy-ui, windows, activity |
| 63 | copy jobs in Activity (new): `Copy` icon, "복사: src.db → tgt.db", counts + [자세히], a warned copy job opening the plan step | JobRow + JobTitle + CopyJobSummary; JobChip tooltip; status pill "이 연결로 복사 중" | activity, copy-ui, windows |
| 64 | write-allowed targets (new): PenLine on rows, 쓰기 허용 해제, never on a new or duplicated connection | Welcome rows and menu, ConnectionDialog, copy result view | windows, welcome, copy-ui |

Nothing is dropped: the header connection select → toolbar switcher + pill + 다른 연결에서 열기…; Extract / Table /
History center tabs → workspace tabs; monitor → inspector Activity (and the Welcome activity popover); bottom log →
console drawer.

---

## 9. File ownership, waves and deliverables

Owner keys (exact); paths relative to `apps/desktop` unless they start with `crates/`.

- **ui-foundation**: `src/app/**`, `src/components/{ui,layout}/**`, `src/stores/**`, `src/lib/**` (incl. `mock-ipc.ts`),
  `src/test/**`, `apps/desktop/DESIGN.md`, apps/desktop config files (eslint, vitest, next, tsconfig, postcss).
- **welcome**: `src/components/welcome/**` (new), `src/components/dialogs/{ConnectionDialog,ImportDbeaverDialog}*`.
- **sidebar**: `src/components/sidebar/**` (new) and `src/components/tree/**` (deleted as its first step).
- **query**: `src/components/extract/**`, moved to `src/components/query/**` (git mv, then delete `extract/`); `ExtractTab`
  → `QueryTabView`, `PreflightDialog` → `PreflightSheet`.
- **structure**: `src/components/{table,history}/**`; `TableTab` → `StructureTabView` + `TableInspector`; `HistoryTab` →
  `HistoryView` + `HistoryList`.
- **activity**: `src/components/{monitor,log,palette}/**`,
  `src/components/dialogs/{Settings,About,HostKey,SshPassword,DbPassword}Dialog*`, `src/components/AppEvents*`;
  `MonitorPanel` → `ActivityPanel`, `LogPanel` → `ConsoleDrawer` + `LogStatusButton`; new `JobChip`, `ActivityPopover`,
  `CommandPalette`.
- **core**: `crates/**`, `packages/ipc-types` (only through `pnpm typegen`), and for this redesign
  `apps/desktop/src-tauri/**`: macOS menu (section 6), title bar and capability (section 10).
- **shared**: everything else: root configs, CI, scripts, `drivers/registry.json`, `CLAUDE.md`.

**Waves.** `tsconfig` includes every `**/*.ts(x)` and `next build` type-checks the whole project, so project-wide gates are
required only at the end.
- **W0 (core)**: `ConnTag` + `Connection.tag` + CLI flags + Rust tests + `pnpm typegen` (7.2); blocks W1. The `src-tauri`
  work may land any time before W3.
- **W1 (ui-foundation, one commit)**: tokens, globals, primitives, stores, lib, mock changes, layout, slot stubs. Keeps
  `layout/HeaderBar.tsx` (re-exporting `StatusDot`), legacy i18n keys and colour aliases.
- **W2 (welcome, sidebar, query, structure, activity in parallel)**: each owner first deletes or moves its own legacy files
  (sidebar `tree/**`; query `extract/**`; structure `TableTab*`, `HistoryTab*`; activity `MonitorPanel*`, `LogPanel*`),
  then builds. Transient errors in other owners' legacy files are expected and not reported.
- **W3 (ui-foundation integration)**: deletes `HeaderBar` (+ test), `CenterTabs`, legacy i18n keys, colour aliases and the
  flat history fields; runs the project-wide gates; reports cross-owner fixes to their owners.

Gates per owner in W1/W2, from the repo root, with the owner's directories as `<dirs>` (e.g. `components/sidebar`): (a)
`pnpm --filter desktop exec tsc --noEmit 2>&1 | grep -E 'src/(<dirs>)/'` prints nothing; (b) `pnpm --filter desktop exec
eslint <dirs>`; (c) `pnpm --filter desktop exec vitest run <dirs>`. W3 requires `pnpm --filter desktop lint`, `pnpm
--filter desktop test` and `pnpm --filter desktop build:web` to pass.

**Cross-owner imports.** Only slot exports (7.6), `@/components/ui`, `@/stores/*`, `@/lib/*` and `ipc-types` cross owner
lines. Known traps: `tree/ConnectionTree.tsx` imports `StatusDot` from `layout/HeaderBar` and `ImportDbeaverDialog` /
`ImportDbeaverSource` from welcome's file (gone once sidebar deletes `tree/**` first); `dialogs/DbPasswordDialog.test.tsx`
renders `<ConnectionTree/>`: activity rewrites it to call `ui.askDbPassword(...)` and spy on `workspace.openWorkspace`,
`catalog.loadDatabases` and `extract.run(tabId)`, importing no other owner's component; the DDL view uses `ui/CodeBlock`,
never `query/SqlEditor`; `ActivityPopover` belongs to activity, not layout.

Deliverables and tests (owners' tests never render AppShell; confirms use `setConfirmHandler`):
- **ui-foundation**: tokens (3), native chrome (3.3), primitives (7.4), stores (7.1), lib (7.2-7.5), layout (`AppShell`,
  `Workspace`, `WorkspaceToolbar`, `StatusPillPopover`, `SidebarFrame`, `WorkspaceTabBar`, `TabContent`, `TableTabFrame`,
  `InspectorFrame`, `ConsoleFrame`, `EmptyWorkspace`, `useWorkspaceShortcuts`), mock tags and `conn_save` tag, slot stubs.
  Tests cover every rule in 7.1 and 7.5, at least: preview replacement id, `newTab`, `seq`, `nextId` reset, table Query
  doc lifecycle; `ensureScope` (`conn.user`), unknown id, A-then-B race, `closeWorkspace` force, deferred
  `openFromHistory`; results stay on their tab; close during explain → reopen → resolve leaves the doc idle; two tabs'
  preflights queue; doc gone at preflight return → no start; settings defaults; DbPassword merge + queue; persist migrate;
  `history_list` gets the connId; shortcuts with `isMac()` mocked both ways (`code` for `₩`, `isComposing`, `ctrl`,
  `menuOwned`, Esc in a SearchField with text leaves `cancelConfirmJobId` unset, F5 in welcome calls `preventDefault`);
  `runAction` errors; `catalog.refresh` invalidates first; `countExactly` confirm / no-confirm + race guard;
  `canTargetTable` (postgres refused, mysql allowed); `rescanDrivers` count; a duplicated savePassword connection prompts
  instead of AUTH; mock `conn_save` tag (set, cleared by omission, duplicate with `id: ''`); primitives (SegmentedControl
  variants + `onActivate`, TabBar keys + middle-click, Popover focus / Esc / portals, KeyHint, RowEstimate); AppShell smoke
  (welcome → workspace → back, slot modules `vi.mock`ed, landmarks only).
- **welcome**: WelcomeScreen, two-step Sheet with TagPicker, ImportDbeaverFlow. Tests: `ConnectionDialog.test.tsx`
  coverage (secrets cleared, clear flags, test saves first, connect opens the workspace, `afterConnectionSaved`), list
  (search, grouping, open, delete confirm, duplicate), import (password-less, unsupported skipped, parse error,
  partial-failure retry), rescan from the 정렬 menu and the form notice.
- **sidebar**: Sidebar. Tests: `ConnectionTree.test.tsx` coverage (estimate labelling in every estimated section, lazy
  load, retry, password prompt, context actions), refresh invalidates then reloads, 탐색 전용 row, make-query disabled for a
  postgres table outside execDb, `sidebarRequest`.
- **query**: QueryTabView, query bar, export panel, PreflightSheet, SqlEditor (`onCheck`, explicit `connId`, `--syn-*`),
  moved plan / completion modules. Tests on the per-tab store: 중지 + inline confirm follow `job:*`, collapsed default,
  warned 작업 strip, sheet queue, resume disables 계속 once the job leaves `warned`.
- **structure**: StructureTabView, TableInspector, HistoryList, HistoryView. Tests: `TableTab.test.tsx` coverage
  (estimate vs exact, stored last count, count race), no IPC for `source: 'selection'`, history (open, run again,
  `history_list` gets the connId, connection Select reloads, another connection's row switches workspaces).
- **activity**: ActivityPanel, JobChip, ActivityPopover, ConsoleDrawer, LogStatusButton, CommandPalette, restyled dialogs,
  AppEvents (no `extract.output` patch; `menu:action`; `history.reloadLoaded`). Tests: ActivityPanel (elapsed + row
  rate), ConsoleDrawer, About without props, HostKey (raw message; Esc and scrim answer false), SshPassword / DbPassword
  (Esc = cancel, resume reasons, merged requests, no `ConnectionTree`), JobChip, palette (run after close, pick mode, `≈ …
  추정` rows).
- **core**: W0 items; `src-tauri` menu (Close Tab ⌘W reaches the WebView as `menu:action`), Overlay title bar, capability.

Rules: keep your gates green; failures in files you do not own go to `notDone` with file, symbol and the change needed. No
`pnpm add`, no edits to `package.json` or the lockfile, no dev server or browser (visual QA is another agent's). Korean by
default, English fallback for every string.

---

## 10. Deviations

- **26px rows instead of CLAUDE.md's "28px rows"** (TablePlus density; 24px menus and section headers, 44px two-line rows).
  Requested from shared: CLAUDE.md Conventions → "13px font, 26px list rows (24px menus/section headers, 44px two-line
  rows)". Until it lands, implementers cite DESIGN.md §3.1 in their summaries.
- **Title bar.** No translucent materials (a WebView cannot draw them): flat greys. macOS (core, `tauri.conf.json`):
  `titleBarStyle: "Overlay"`, `hiddenTitle: true` (plus `trafficLightPosition` to centre the lights in the 40px toolbar if
  the pinned Tauri supports it); toolbar `padding-left: 78px` when `isMac()`, `data-tauri-drag-region` on empty toolbar
  space and the Welcome backdrop strip, capability `core:window:allow-start-dragging` (added with the Overlay bar).
  Windows: native decorations, no drag region, `--bg-toolbar` = the Win11 caption colour (one band).
- No user-defined connection groups (no core field); Welcome groups by tag or driver. Status colour = tag colour.
- No Safe Mode, row limit, Beautify, saved queries, metrics, data / JSON views (not BulkDB features; data is forbidden).
- Inspector toggle `Mod+\` (TablePlus' Space collides with typing); connection switch `Mod+Shift+O` because `Mod+Shift+K` is
  CodeMirror's delete-line and `Mod+[` / `Mod+]` its indent keys (tabs use `Ctrl+Tab`, `Mod+1…9`).
- `Esc` asks to cancel the newest job (BulkDB-specific, only when nothing local consumed it); `Mod+.` is the
  macOS-standard Stop on both OSes. Hangul is never below 12px (Malgun Gothic blurs at 11px at 100% Windows scaling).
- `Popover` is hand-built: `@radix-ui/react-popover` is not a dependency.
- 복제 (duplicate) is a UI composition over `conn_save`; the CLI covers it with `conn show` + `conn add` (no `conn
  duplicate`), which satisfies CLAUDE.md rule 3.
- **W1 kept the old panels alive.** 7.6 asked for EmptyState stubs and 7.1 removes the flat extract / ui fields in W1;
  the foundation instead kept a deprecated compatibility layer and made the stubs render the current components (7.7),
  so `pnpm --filter desktop test` and `build:web` stay green project-wide and the app works end to end during W2.
- The mock backend has a third connection, `c-staging` (`staging (PostgreSQL)`, tag staging), next to prod
  (production) and local (local). It also lists the two MySQL demo servers of the copy (not part of `mockConnections()`):
  `c-legacy` (MySQL 4.0, EUC-KR, testing; databases `shop` and `board`, MyISAM, Korean table comments) and `c-mysql8`
  (MySQL 8.0, development, write-allowed; database `app`), so the legacy → utf8mb4 copy can be tried end to end. A mock
  copy run takes about 20-30 s (`progressTickMs` × 3 per step) so its progress view can be watched.
- docs/db-copy.md §7 names a `SegmentedControl` for 전체 / 테이블 선택; the wizard uses an inline radio row instead (the
  copy wizard look under §7.6), so it cannot be mistaken for the steps bar.
- In mock / browser mode `Mod+T/W/N` and `Ctrl+Tab` are reserved by the browser. In Tauri on macOS, until core's custom
  menu lands, `⌘W` (Tauri's default Close Window) closes the whole window instead of a tab, and other default key
  equivalents run before the WebView.
