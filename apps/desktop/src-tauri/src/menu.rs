//! macOS menu bar (DESIGN.md §6). A custom menu replaces Tauri's default one, whose Close
//! Window item owns ⌘W and would close the app instead of a tab.
//!
//! Every custom item's menu id is a frontend action id: choosing it, or pressing its key
//! equivalent, sends `menu:action` with that id to the focused window only, whose `AppEvents`
//! calls `runAction(id)` (a disabled action is a no-op there). The one exception is Close
//! Window (⇧⌘W), which Rust handles itself: it closes the focused window. Inside Tauri on macOS the JS dispatcher skips the KEYMAP
//! rows marked `menuOwned` (`src/lib/keymap.ts`), so this table must give every one of them a
//! menu item with the same key, and no other key: the tests below read keymap.ts to check that.
//!
//! Windows has no menu bar: `install` does nothing there and the JS dispatcher handles every
//! key. The OS test is a runtime comparison with `bulk_core::platform::os_key()`, so the only
//! `#[cfg(target_os)]` stays in core's platform module (CLAUDE.md rule 5).
//!
//! Labels follow the UI language: Korean (the UI default) at startup; the frontend relabels the
//! menu through the `menu_set_language` command when the language changes.

use tauri::menu::{Menu, MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::{AppHandle, Runtime};

/// Event carrying a menu item's action id to the webview (payload: the id as a string).
pub const MENU_ACTION_EVENT: &str = "menu:action";

/// Menu id of the custom Close Window item (handled in Rust, never emitted).
const CLOSE_WINDOW: &str = "window.close";

/// UI language of the menu labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ko,
    En,
}

impl Lang {
    /// `ko` | `en` (the frontend's `Lang` values).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "ko" => Some(Self::Ko),
            "en" => Some(Self::En),
            _ => None,
        }
    }
}

/// A label in both UI languages.
#[derive(Debug, Clone, Copy)]
struct Label {
    ko: &'static str,
    en: &'static str,
}

impl Label {
    fn get(self, lang: Lang) -> &'static str {
        match lang {
            Lang::Ko => self.ko,
            Lang::En => self.en,
        }
    }
}

const fn l(ko: &'static str, en: &'static str) -> Label {
    Label { ko, en }
}

/// Native items: macOS implements them (responder chain); WKWebView needs the Edit ones for
/// its clipboard and undo keys. Their key equivalents come from the OS.
#[derive(Debug, Clone, Copy)]
enum Native {
    Services,
    Hide,
    HideOthers,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Zoom,
}

#[derive(Debug, Clone, Copy)]
enum Entry {
    /// A custom item: menu id (a frontend action id, or `CLOSE_WINDOW`), label and key
    /// equivalent in Tauri accelerator syntax.
    Item {
        id: &'static str,
        label: Label,
        accel: Option<&'static str>,
    },
    Native(Native, Label),
    Separator,
}

const fn item(id: &'static str, label: Label, accel: Option<&'static str>) -> Entry {
    Entry::Item { id, label, accel }
}

struct Section {
    title: Label,
    entries: &'static [Entry],
}

/// The whole menu bar, in order (DESIGN.md §6). The first submenu is the app menu (macOS shows
/// the app name as its title).
const MENU: &[Section] = &[
    Section {
        title: l("BulkDB", "BulkDB"),
        entries: &[
            item("app.about", l("BulkDB 정보", "About BulkDB"), None),
            Entry::Separator,
            item("app.settings", l("설정…", "Settings…"), Some("CmdOrCtrl+,")),
            Entry::Separator,
            Entry::Native(Native::Services, l("서비스", "Services")),
            Entry::Separator,
            Entry::Native(Native::Hide, l("BulkDB 가리기", "Hide BulkDB")),
            Entry::Native(Native::HideOthers, l("기타 가리기", "Hide Others")),
            Entry::Separator,
            Entry::Native(Native::Quit, l("BulkDB 종료", "Quit BulkDB")),
        ],
    },
    Section {
        title: l("파일", "File"),
        entries: &[
            item("connection.new", l("새 연결", "New Connection"), Some("CmdOrCtrl+N")),
            item("window.new", l("새 창", "New Window"), Some("CmdOrCtrl+Shift+N")),
            // `open -n -a BulkDB` and the CLI start another instance too (docs/db-copy.md 1.2).
            item("app.newInstance", l("새 BulkDB 실행", "New BulkDB Instance"), None),
            item("tabs.newQuery", l("새 쿼리", "New Query"), Some("CmdOrCtrl+T")),
            item("query.openFile", l("SQL 파일 열기…", "Open SQL File…"), Some("CmdOrCtrl+O")),
            Entry::Separator,
            item("copy.open", l("데이터베이스 복사…", "Copy Database…"), None),
            Entry::Separator,
            item("tabs.close", l("탭 닫기", "Close Tab"), Some("CmdOrCtrl+W")),
            item(CLOSE_WINDOW, l("창 닫기", "Close Window"), Some("CmdOrCtrl+Shift+W")),
        ],
    },
    Section {
        title: l("편집", "Edit"),
        entries: &[
            Entry::Native(Native::Undo, l("실행 취소", "Undo")),
            Entry::Native(Native::Redo, l("실행 복귀", "Redo")),
            Entry::Separator,
            Entry::Native(Native::Cut, l("오려두기", "Cut")),
            Entry::Native(Native::Copy, l("복사하기", "Copy")),
            Entry::Native(Native::Paste, l("붙여넣기", "Paste")),
            Entry::Native(Native::SelectAll, l("전체 선택", "Select All")),
        ],
    },
    Section {
        title: l("보기", "View"),
        entries: &[
            item("view.toggleSidebar", l("사이드바 보기/숨기기", "Toggle Sidebar"), Some("CmdOrCtrl+0")),
            item("view.toggleInspector", l("인스펙터 보기/숨기기", "Toggle Inspector"), Some("CmdOrCtrl+\\")),
            item("view.toggleConsole", l("콘솔 보기/숨기기", "Console"), Some("CmdOrCtrl+J")),
            Entry::Separator,
            item("catalog.refresh", l("새로 고침", "Refresh"), Some("CmdOrCtrl+R")),
            item("palette.open", l("명령 팔레트", "Command Palette"), Some("CmdOrCtrl+P")),
        ],
    },
    Section {
        title: l("쿼리", "Query"),
        entries: &[
            item("query.run", l("실행", "Run"), Some("CmdOrCtrl+Enter")),
            item("query.explain", l("계획 보기", "Explain"), Some("CmdOrCtrl+E")),
            item("query.check", l("SQL 검사", "Check"), Some("CmdOrCtrl+Shift+Enter")),
            Entry::Separator,
            item("jobs.cancelAsk", l("작업 중지", "Stop"), Some("CmdOrCtrl+.")),
        ],
    },
    Section {
        title: l("윈도우", "Window"), entries: &[Entry::Native(Native::Minimize, l("최소화", "Minimize")), Entry::Native(Native::Zoom, l("확대/축소", "Zoom"))]
    },
];

/// True where the app has a menu bar: macOS only (Windows has none, DESIGN.md §6).
pub fn has_menu_bar() -> bool {
    bulk_core::platform::os_key() == "macos"
}

/// Set (or replace, to relabel) the app menu. Does nothing without a menu bar.
pub fn install<R: Runtime>(app: &AppHandle<R>, lang: Lang) -> tauri::Result<()> {
    if has_menu_bar() {
        app.set_menu(build(app, lang)?)?;
    }
    Ok(())
}

fn build<R: Runtime>(app: &AppHandle<R>, lang: Lang) -> tauri::Result<Menu<R>> {
    let mut menu = MenuBuilder::new(app);
    for section in MENU {
        let mut sub = SubmenuBuilder::new(app, section.title.get(lang));
        for entry in section.entries {
            sub = match *entry {
                Entry::Item { id, label, accel } => sub.item(&MenuItem::with_id(app, id, label.get(lang), true, accel)?),
                Entry::Separator => sub.separator(),
                Entry::Native(kind, label) => {
                    let text = label.get(lang);
                    match kind {
                        Native::Services => sub.services_with_text(text),
                        Native::Hide => sub.hide_with_text(text),
                        Native::HideOthers => sub.hide_others_with_text(text),
                        Native::Quit => sub.quit_with_text(text),
                        Native::Undo => sub.undo_with_text(text),
                        Native::Redo => sub.redo_with_text(text),
                        Native::Cut => sub.cut_with_text(text),
                        Native::Copy => sub.copy_with_text(text),
                        Native::Paste => sub.paste_with_text(text),
                        Native::SelectAll => sub.select_all_with_text(text),
                        Native::Minimize => sub.minimize_with_text(text),
                        // NSWindow's zoom (performZoom:) is muda's "maximize" item on macOS.
                        Native::Zoom => sub.maximize_with_text(text),
                    }
                }
            };
        }
        menu = menu.item(&sub.build()?);
    }
    menu.build()
}

/// What a menu event does.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    /// Emit `menu:action` with this action id.
    Action(&'a str),
    CloseWindow,
}

/// Custom items, in menu order: `(id, accelerator)`.
fn custom_items() -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
    MENU.iter().flat_map(|s| s.entries.iter()).filter_map(|e| match *e {
        Entry::Item { id, accel, .. } => Some((id, accel)),
        _ => None,
    })
}

/// Native items' events (auto-generated ids) and anything unknown route nowhere.
fn route(id: &str) -> Option<Route<'_>> {
    if id == CLOSE_WINDOW {
        return Some(Route::CloseWindow);
    }
    custom_items().any(|(item_id, _)| item_id == id).then_some(Route::Action(id))
}

/// `Builder::on_menu_event` handler. Actions go to the focused window only (`emit_to`; an
/// `emit` would run the action in every window); Close Window closes the focused window.
pub fn on_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match route(id) {
        Some(Route::Action(action)) => crate::windows::emit_to_focused(app, MENU_ACTION_EVENT, action),
        Some(Route::CloseWindow) => crate::windows::close_focused(app),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The frontend keymap, as the JS dispatcher reads it.
    const KEYMAP_TS: &str = include_str!("../../src/lib/keymap.ts");
    /// The frontend action registry `runAction` looks ids up in.
    const BUILTIN_ACTIONS_TS: &str = include_str!("../../src/lib/builtin-actions.ts");

    /// The single-quoted string after `key: '` on `line` (JS escapes undone for `\\` and `\'`).
    fn quoted(line: &str, key: &str) -> Option<String> {
        let start = line.find(&format!("{key}: '"))? + key.len() + 3;
        let rest = &line[start..];
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?),
                '\'' => return Some(out),
                c => out.push(c),
            }
        }
        None
    }

    /// `(action, combo)` of every KEYMAP row marked `menuOwned: true`.
    fn menu_owned_rows() -> BTreeSet<(String, String)> {
        let rows: BTreeSet<_> = KEYMAP_TS
            .lines()
            .filter(|line| line.contains("menuOwned: true"))
            .map(|line| (quoted(line, "action").expect("action on a menuOwned row"), quoted(line, "combo").expect("combo on a menuOwned row")))
            .collect();
        assert!(rows.len() >= 10, "keymap.ts parse found only {} menuOwned rows: {rows:?}", rows.len());
        rows
    }

    /// A keymap combo (`mod+shift+enter`) in Tauri accelerator syntax (`CmdOrCtrl+Shift+Enter`).
    fn accelerator(combo: &str) -> String {
        // A trailing `+` key (`mod++`) would need care; the keymap has none.
        combo
            .split('+')
            .map(|part| match part {
                "mod" => "CmdOrCtrl".to_string(),
                "shift" => "Shift".to_string(),
                "alt" => "Alt".to_string(),
                "ctrl" => "Ctrl".to_string(),
                "enter" => "Enter".to_string(),
                key => key.to_uppercase(),
            })
            .collect::<Vec<_>>()
            .join("+")
    }

    #[test]
    fn every_menu_owned_shortcut_has_a_menu_item_with_the_same_key() {
        let owned: BTreeSet<(String, String)> = menu_owned_rows().into_iter().map(|(action, combo)| (action, accelerator(&combo))).collect();
        let menu: BTreeSet<(String, String)> =
            custom_items().filter(|(id, _)| *id != CLOSE_WINDOW).filter_map(|(id, accel)| accel.map(|a| (id.to_string(), a.to_string()))).collect();
        // Missing from the menu: the key does nothing on macOS. Extra in the menu: it fires twice
        // (menu and JS dispatcher) or shadows a key the JS dispatcher owns.
        assert_eq!(menu, owned, "macOS menu key equivalents must equal the menuOwned KEYMAP rows");
    }

    #[test]
    fn accelerators_use_keys_the_tauri_parser_accepts() {
        // muda 0.20's parser: these modifier and key names (others are silently dropped).
        for (id, accel) in custom_items() {
            let Some(accel) = accel else { continue };
            let parts: Vec<&str> = accel.split('+').collect();
            let (key, mods) = parts.split_last().expect("non-empty accelerator");
            assert!(mods.iter().all(|m| ["CmdOrCtrl", "Shift", "Alt", "Ctrl"].contains(m)), "{id}: modifiers of {accel}");
            assert!(mods.contains(&"CmdOrCtrl"), "{id}: {accel} needs ⌘");
            let single = key.len() == 1 && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || ",.\\".contains(c));
            assert!(single || *key == "Enter", "{id}: key of {accel}");
        }
    }

    #[test]
    fn every_emitted_action_is_a_registered_frontend_action() {
        let registered: BTreeSet<String> = BUILTIN_ACTIONS_TS.lines().filter_map(|line| quoted(line, "id")).collect();
        assert!(registered.len() >= 20, "builtin-actions.ts parse found only {} ids", registered.len());
        for (id, _) in custom_items().filter(|(id, _)| *id != CLOSE_WINDOW) {
            assert!(registered.contains(id), "menu item {id} is not a builtin action");
        }
    }

    #[test]
    fn layout_follows_the_design() {
        let titles: Vec<&str> = MENU.iter().map(|s| s.title.en).collect();
        assert_eq!(titles, ["BulkDB", "File", "Edit", "View", "Query", "Window"]);
        // Close Tab owns ⌘W (Tauri's default Close Window would quit the app).
        assert!(custom_items().any(|i| i == ("tabs.close", Some("CmdOrCtrl+W"))));
        assert!(custom_items().any(|i| i == (CLOSE_WINDOW, Some("CmdOrCtrl+Shift+W"))));
        assert!(custom_items().any(|i| i == ("app.about", None)));
        // WKWebView's clipboard and undo keys need the native Edit items.
        let edit: Vec<&str> = MENU[2].entries.iter().filter_map(|e| if let Entry::Native(_, label) = e { Some(label.en) } else { None }).collect();
        assert_eq!(edit, ["Undo", "Redo", "Cut", "Copy", "Paste", "Select All"]);
    }

    #[test]
    fn ids_are_unique_and_labels_exist_in_both_languages() {
        let ids: Vec<&str> = custom_items().map(|(id, _)| id).collect();
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len(), "duplicate menu ids: {ids:?}");
        for section in MENU {
            assert!(!section.title.ko.is_empty() && !section.title.en.is_empty());
            for entry in section.entries {
                if let Entry::Item { label, .. } | Entry::Native(_, label) = entry {
                    assert!(!label.ko.trim().is_empty() && !label.en.trim().is_empty(), "{label:?}");
                }
            }
        }
    }

    /// docs/db-copy.md 1.1: File gains 새 창 (⇧⌘N), 새 BulkDB 실행 and 데이터베이스 복사… (no key).
    #[test]
    fn file_menu_has_the_window_instance_and_copy_items() {
        let file: Vec<(&str, Option<&str>)> = MENU[1]
            .entries
            .iter()
            .filter_map(|e| match *e {
                Entry::Item { id, accel, .. } => Some((id, accel)),
                _ => None,
            })
            .collect();
        assert!(file.contains(&("window.new", Some("CmdOrCtrl+Shift+N"))));
        assert!(file.contains(&("app.newInstance", None)));
        assert!(file.contains(&("copy.open", None)));
        assert_eq!(route("window.new"), Some(Route::Action("window.new")));
        assert_eq!(route("copy.open"), Some(Route::Action("copy.open")));
    }

    #[test]
    fn routes() {
        assert_eq!(route("tabs.close"), Some(Route::Action("tabs.close")));
        assert_eq!(route("query.run"), Some(Route::Action("query.run")));
        assert_eq!(route(CLOSE_WINDOW), Some(Route::CloseWindow));
        // Native items and anything unknown emit nothing.
        assert_eq!(route("1"), None);
        assert_eq!(route("tabs.select1"), None);
        assert_eq!(route(""), None);
    }

    #[test]
    fn lang_parse() {
        assert_eq!(Lang::parse("ko"), Some(Lang::Ko));
        assert_eq!(Lang::parse(" en "), Some(Lang::En));
        assert_eq!(Lang::parse("fr"), None);
        assert_eq!(l("가", "a").get(Lang::Ko), "가");
        assert_eq!(l("가", "a").get(Lang::En), "a");
    }

    #[test]
    fn menu_bar_only_on_macos() {
        assert_eq!(has_menu_bar(), std::env::consts::OS == "macos");
    }
}
