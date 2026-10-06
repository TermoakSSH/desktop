//! The menu of the app icon in the macOS Dock (right click): a new local
//! terminal, quick connect, a new window, and the Hosts and Server sessions
//! sections. Each entry brings the app to the front first, and opens a
//! window if every one was closed.
//!
//! The Dock menu dispatches its actions to the window in front, or to the
//! app when none is (the usual case: the app is in the background), so its
//! actions are handled by the app ([`init`]) and not by a window; that also
//! keeps them enabled with no window open. Other platforms have no Dock
//! menu: the actions exist but nothing shows them.

use gpui::{Action, App, MenuItem, actions};

use crate::app::{NewWindow, Section};
use crate::views::OpenRequest;
use crate::views::host_picker::PickMode;
use crate::windows;

actions!(
    dock,
    [
        /// A tab with a terminal of this computer.
        NewTerminal,
        /// The host picker, to connect somewhere.
        QuickConnect,
        /// The Hosts section.
        ShowHosts,
        /// The Server sessions section.
        ShowServerSessions
    ]
);

/// Entries of the Dock menu: translation key of the label and its action
/// (`None`: a separator).
pub fn spec() -> Vec<Option<(&'static str, Box<dyn Action>)>> {
    vec![
        Some(("menu.new_local_terminal", Box::new(NewTerminal))),
        Some(("menu.quick_connect", Box::new(QuickConnect))),
        Some(("menu.new_window", Box::new(NewWindow))),
        None,
        Some(("app.section.hosts", Box::new(ShowHosts))),
        Some(("app.section.server_sessions", Box::new(ShowServerSessions))),
    ]
}

/// What the Dock menu does (the app's own handlers; `NewWindow` already is
/// one, in `app::init`).
pub fn init(cx: &mut App) {
    cx.on_action(|_: &NewTerminal, cx: &mut App| {
        windows::with_front_window(cx, |app, window, cx| {
            app.open(OpenRequest::Shell, window, cx)
        })
    });
    cx.on_action(|_: &QuickConnect, cx: &mut App| {
        windows::with_front_window(cx, |app, window, cx| {
            app.open_host_picker(PickMode::Quick, window, cx)
        })
    });
    cx.on_action(|_: &ShowHosts, cx: &mut App| {
        windows::with_front_window(cx, |app, window, cx| {
            app.select_section(Section::Hosts, window, cx)
        })
    });
    cx.on_action(|_: &ShowServerSessions, cx: &mut App| {
        windows::with_front_window(cx, |app, window, cx| {
            app.select_section(Section::ServerSessions, window, cx)
        })
    });
}

/// Sets the Dock menu in the current language (macOS; called again when
/// the language changes).
pub fn set_menu(cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let items = spec()
        .into_iter()
        .map(|e| match e {
            Some((label, action)) => MenuItem::Action {
                name: t!(label),
                action,
                os_action: None,
                checked: false,
                disabled: false,
            },
            None => MenuItem::separator(),
        })
        .collect();
    cx.set_dock_menu(items);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_translated() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("locales/en.json");
        let en: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let entries = spec();
        assert!(entries.first().is_some_and(Option::is_some));
        assert!(entries.last().is_some_and(Option::is_some));
        for (label, _) in entries.into_iter().flatten() {
            assert!(en.contains_key(label), "{label}");
        }
    }
}
