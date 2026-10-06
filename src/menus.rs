//! Application menus: the macOS menu bar and, on Windows and Linux (which
//! have no global menu bar in GPUI), the same menus behind the ☰ button of
//! the title bar.
//!
//! One description ([`spec`]) serves both: each entry has a translation key,
//! the action it dispatches and what it needs to be enabled. On macOS GPUI
//! disables the items whose action has no handler where the focus is (the
//! window registers the handlers only when they have a target); the ☰ menu
//! uses [`Need`] with the window's [`MenuState`].

use gpui::{Action, App, Menu, MenuItem, SharedString};

use crate::app::{
    About, AddPane, CheckForUpdates, ClosePane, CloseTab, DuplicateSession, FocusPaneDown,
    FocusPaneLeft, FocusPaneRight, FocusPaneUp, GoHome, Hide, HideOthers, Minimize, NewHost,
    NewLocalTerminal, NewTab, NewWindow, NextTab, OpenDocs, OpenSettings, OpenSftp, PrevTab,
    QuickConnect, Quit, Reconnect, ReportIssue, SendSnippet, ShowAll, ShowShortcuts,
    ToggleBroadcast, ToggleCopilot, ToggleFocusMode, ToggleFullScreen, ToggleSidebar, ZoomIn,
    ZoomOut, ZoomReset,
};
use crate::terminal;

/// Project page (Help → Documentation).
pub const DOCS_URL: &str = "https://github.com/TermoakSSH/desktop";
/// Where to report problems (Help → Report an issue).
pub const ISSUES_URL: &str = "https://github.com/TermoakSSH/desktop/issues";

/// What an entry needs to be enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Nothing,
    /// A terminal tab is active.
    Terminal,
    /// The active tab is a split view (two or more panes).
    Split,
    /// The focused terminal can be written to.
    Writable,
    /// The focused terminal has text selected.
    Selection,
    /// The focused terminal ended (closed or failed).
    Ended,
    /// The focused terminal is connected to a host (SFTP).
    Host,
    /// The active tab can be opened again (an SFTP browser, or a terminal
    /// that is not an attached session without a host).
    Duplicable,
    /// Some tab is open.
    AnyTab,
    /// Automatic updates are enabled in this build.
    Updates,
}

/// What the window has now, to enable the entries of the ☰ menu.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuState {
    pub terminal: bool,
    pub split: bool,
    pub writable: bool,
    pub selection: bool,
    pub ended: bool,
    pub host: bool,
    pub duplicable: bool,
    pub any_tab: bool,
    pub updates: bool,
}

impl MenuState {
    pub fn allows(&self, need: Need) -> bool {
        match need {
            Need::Nothing => true,
            Need::Terminal => self.terminal,
            Need::Split => self.split,
            Need::Writable => self.writable,
            Need::Selection => self.selection,
            Need::Ended => self.ended,
            Need::Host => self.host,
            Need::Duplicable => self.duplicable,
            Need::AnyTab => self.any_tab,
            Need::Updates => self.updates,
        }
    }
}

/// An entry of a menu.
pub enum Entry {
    Item {
        /// Translation key of the label.
        label: &'static str,
        action: Box<dyn Action>,
        need: Need,
    },
    Separator,
}

/// A menu of the bar.
pub struct MenuSpec {
    /// Translation key of the title (`None`: the app name, "Termoak").
    pub title: Option<&'static str>,
    pub entries: Vec<Entry>,
}

fn item(label: &'static str, action: impl Action, need: Need) -> Entry {
    Entry::Item {
        label,
        action: Box::new(action),
        need,
    }
}

/// The menus. `macos` adds the items only the macOS app menu has (Hide,
/// Hide others, Show all, Minimize) and puts About, Settings and Quit in the
/// Termoak menu as the platform expects; elsewhere the first menu is "File"
/// with them.
pub fn spec(macos: bool) -> Vec<MenuSpec> {
    use Entry::Separator;
    use Need::*;
    let mut menus = Vec::new();

    let mut app = vec![
        item("menu.about", About, Nothing),
        item("menu.check_updates", CheckForUpdates, Updates),
        Separator,
        item("menu.settings", OpenSettings, Nothing),
    ];
    if macos {
        app.extend([
            Separator,
            item("menu.hide", Hide, Nothing),
            item("menu.hide_others", HideOthers, Nothing),
            item("menu.show_all", ShowAll, Nothing),
        ]);
    }
    app.extend([Separator, item("menu.quit", Quit, Nothing)]);
    menus.push(MenuSpec {
        title: None,
        entries: app,
    });

    menus.push(MenuSpec {
        title: Some("menu.file"),
        entries: vec![
            item("menu.new_window", NewWindow, Nothing),
            item("menu.new_tab", NewTab, Nothing),
            item("menu.new_local_terminal", NewLocalTerminal, Nothing),
            item("menu.new_host", NewHost, Nothing),
            item("menu.quick_connect", QuickConnect, Nothing),
            Separator,
            item("menu.close_pane", ClosePane, Split),
            item("menu.close_tab", CloseTab, AnyTab),
        ],
    });

    menus.push(MenuSpec {
        title: Some("menu.edit"),
        entries: vec![
            item("menu.copy", terminal::Copy, Selection),
            item("menu.paste", terminal::Paste, Writable),
            item("menu.paste_selection", terminal::PasteSelection, Selection),
            item("menu.select_all", terminal::SelectAll, Terminal),
            Separator,
            item("menu.find", terminal::Find, Terminal),
            item("menu.clear", terminal::ClearTerminal, Terminal),
        ],
    });

    menus.push(MenuSpec {
        title: Some("menu.view"),
        entries: vec![
            item("menu.home", GoHome, Nothing),
            item("menu.toggle_sidebar", ToggleSidebar, Nothing),
            item("menu.copilot", ToggleCopilot, Terminal),
            Separator,
            item("menu.add_pane", AddPane, Terminal),
            item("menu.focus_mode", ToggleFocusMode, Split),
            item("menu.pane_left", FocusPaneLeft, Split),
            item("menu.pane_right", FocusPaneRight, Split),
            item("menu.pane_up", FocusPaneUp, Split),
            item("menu.pane_down", FocusPaneDown, Split),
            Separator,
            item("menu.zoom_in", ZoomIn, Nothing),
            item("menu.zoom_out", ZoomOut, Nothing),
            item("menu.zoom_reset", ZoomReset, Nothing),
            Separator,
            item("menu.full_screen", ToggleFullScreen, Nothing),
        ],
    });

    menus.push(MenuSpec {
        title: Some("menu.terminal"),
        entries: vec![
            item("menu.broadcast", ToggleBroadcast, Split),
            item("menu.send_snippet", SendSnippet, Writable),
            Separator,
            item("menu.reconnect", Reconnect, Ended),
            item("menu.open_sftp", OpenSftp, Host),
            item("menu.duplicate_session", DuplicateSession, Duplicable),
        ],
    });

    let mut window = vec![
        item("menu.next_tab", NextTab, AnyTab),
        item("menu.previous_tab", PrevTab, AnyTab),
    ];
    if macos {
        window.extend([Separator, item("menu.minimize", Minimize, Nothing)]);
    }
    menus.push(MenuSpec {
        title: Some("menu.window"),
        entries: window,
    });

    menus.push(MenuSpec {
        title: Some("menu.help"),
        entries: vec![
            item("menu.docs", OpenDocs, Nothing),
            item("menu.report_issue", ReportIssue, Nothing),
            Separator,
            item("menu.shortcuts", ShowShortcuts, Nothing),
        ],
    });
    menus
}

/// Title of a menu in the current language.
pub fn title(spec: &MenuSpec) -> SharedString {
    match spec.title {
        Some(key) => t!(key),
        None => "Termoak".into(),
    }
}

/// Sets the menu bar (macOS; elsewhere GPUI keeps it for the ☰ menu), in
/// the current language. Called again when the language changes.
pub fn set_menus(cx: &mut App) {
    let menus = spec(cfg!(target_os = "macos"))
        .into_iter()
        .map(|m| Menu {
            name: title(&m),
            items: m
                .entries
                .into_iter()
                .map(|e| match e {
                    Entry::Separator => MenuItem::separator(),
                    Entry::Item { label, action, .. } => MenuItem::Action {
                        name: t!(label),
                        action,
                        os_action: None,
                        checked: false,
                        disabled: false,
                    },
                })
                .collect(),
            disabled: false,
        })
        .collect::<Vec<_>>();
    cx.set_menus(menus);
}

/// Keyboard shortcuts for Help → Keyboard shortcuts: translation key of the
/// description, macOS keys and Windows/Linux keys.
pub fn shortcuts() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("shortcuts.new_window", "⌘N", "—"),
        ("shortcuts.new_tab", "⌘T", "Ctrl+T"),
        ("shortcuts.local_terminal", "⌘⇧T", "Ctrl+Shift+T"),
        ("shortcuts.close_tab", "⌘W", "Ctrl+Shift+W"),
        ("shortcuts.next_tab", "⌘⇧] · ⌃Tab", "Ctrl+Tab"),
        ("shortcuts.previous_tab", "⌘⇧[ · ⌃⇧Tab", "Ctrl+Shift+Tab"),
        ("shortcuts.home", "⌘1", "Ctrl+Shift+H"),
        ("shortcuts.settings", "⌘,", "Ctrl+,"),
        ("shortcuts.new_host", "⌘⇧N", "Ctrl+Shift+N"),
        ("shortcuts.copy", "⌘C", "Ctrl+Shift+C"),
        ("shortcuts.paste", "⌘V", "Ctrl+Shift+V · Shift+Insert"),
        ("shortcuts.select_all", "⌘A", "Ctrl+Shift+A"),
        ("shortcuts.find", "⌘F", "Ctrl+Shift+F"),
        ("shortcuts.clear", "⌘K", "Ctrl+Shift+K"),
        ("shortcuts.add_pane", "⌘D", "Ctrl+Shift+D"),
        ("shortcuts.focus_mode", "⌘⇧M", "Ctrl+Shift+M"),
        ("shortcuts.move_pane", "⌘⌥←↑→↓", "Ctrl+Alt+←↑→↓"),
        ("shortcuts.close_pane", "⌘⌥W", "—"),
        ("shortcuts.broadcast", "⌘B", "Ctrl+Alt+B"),
        ("shortcuts.send_snippet", "⌘⇧S", "Ctrl+Shift+S"),
        ("shortcuts.reconnect", "⌘⇧R", "Ctrl+Shift+R"),
        ("shortcuts.zoom", "⌘= · ⌘- · ⌘0", "Ctrl+= · Ctrl+- · Ctrl+0"),
        ("shortcuts.full_screen", "⌃⌘F", "F11"),
        ("shortcuts.copilot", "⌘I", "Ctrl+Shift+I"),
        ("shortcuts.host_menu", "⇧F10", "Shift+F10 · Menu"),
        (
            "shortcuts.scroll",
            "⇧PgUp · ⇧PgDn · ⇧End",
            "Shift+PgUp · Shift+PgDn · Shift+End",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english() -> serde_json::Map<String, serde_json::Value> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales/en.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn every_label_is_translated() {
        let en = english();
        for macos in [true, false] {
            for menu in spec(macos) {
                if let Some(key) = menu.title {
                    assert!(en.contains_key(key), "{key}");
                }
                for e in &menu.entries {
                    if let Entry::Item { label, .. } = e {
                        assert!(en.contains_key(*label), "{label}");
                    }
                }
            }
        }
        for (key, mac, other) in shortcuts() {
            assert!(en.contains_key(key), "{key}");
            assert!(!mac.is_empty() && !other.is_empty());
        }
    }

    #[test]
    fn menu_bar_layout() {
        let names =
            |macos| -> Vec<Option<&'static str>> { spec(macos).iter().map(|m| m.title).collect() };
        let expected = vec![
            None,
            Some("menu.file"),
            Some("menu.edit"),
            Some("menu.view"),
            Some("menu.terminal"),
            Some("menu.window"),
            Some("menu.help"),
        ];
        assert_eq!(names(true), expected);
        assert_eq!(names(false), expected);
        let labels = |macos| -> Vec<&'static str> {
            spec(macos)
                .into_iter()
                .flat_map(|m| m.entries)
                .filter_map(|e| match e {
                    Entry::Item { label, .. } => Some(label),
                    Entry::Separator => None,
                })
                .collect()
        };
        let mac = labels(true);
        let other = labels(false);
        // Only macOS has Hide, Show all and Minimize.
        for only_mac in [
            "menu.hide",
            "menu.hide_others",
            "menu.show_all",
            "menu.minimize",
        ] {
            assert!(mac.contains(&only_mac));
            assert!(!other.contains(&only_mac));
        }
        for both in ["menu.quit", "menu.settings", "menu.broadcast", "menu.find"] {
            assert!(mac.contains(&both) && other.contains(&both));
        }
        // No label twice.
        let mut sorted = mac.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), mac.len());
        // No menu starts or ends with a separator, nor has two in a row.
        for menu in spec(true).into_iter().chain(spec(false)) {
            let seps: Vec<bool> = menu
                .entries
                .iter()
                .map(|e| matches!(e, Entry::Separator))
                .collect();
            assert!(!seps.first().unwrap() && !seps.last().unwrap());
            assert!(!seps.windows(2).any(|w| w[0] && w[1]));
        }
    }

    #[test]
    fn enabled_entries() {
        let idle = MenuState::default();
        assert!(idle.allows(Need::Nothing));
        assert!(!idle.allows(Need::Split));
        assert!(!idle.allows(Need::Writable));
        let split = MenuState {
            terminal: true,
            split: true,
            any_tab: true,
            ..Default::default()
        };
        assert!(split.allows(Need::Split) && split.allows(Need::Terminal));
        assert!(!split.allows(Need::Ended));
    }
}
