//! Termoak for desktop: native SSH client (GPUI, no webviews) for Windows,
//! Linux and macOS, in the style of Termius.
//!
//! Startup:
//! 1. The update downloaded in the previous session (if any) is applied and
//!    the app relaunches already updated, before opening any window.
//! 2. The tokio runtime used by the SSH engine and the API is created.
//! 3. The local vault is opened. Its key is in the system keychain, read
//!    once (`termoak_client::vault::load_or_create_key`); if the keychain
//!    refuses and there is already data, an error screen offers to try
//!    again instead of inventing a key.
//! 4. The interface language is chosen and the window is opened (File →
//!    New window opens more on the same data, see `windows.rs`; on macOS
//!    the Dock icon brings it back after closing it).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// Translations in `locales/<lang>.json` (see docs/I18N.md).
rust_i18n::i18n!("locales", fallback = "en");

// First, so that `t!` and `tn!` are available in every module.
#[macro_use]
mod i18n;

mod accounts;
mod app;
mod dock;
mod drag;
mod importers;
mod links;
mod local_ai;
mod menus;
mod notifications;
mod panes;
mod prompts;
mod qr;
mod runtime;
mod sharing;
mod state;
mod terminal;
mod theme;
mod ui;
mod update;
mod views;
mod windows;

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, ClickEvent, Context, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder, px,
    size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Root, StyledExt, TitleBar, h_flex, v_flex};
use termoak_client::Workspace;

use crate::app::AppView;
use crate::prompts::DesktopPrompter;
use crate::state::{AppModel, Settings};
use crate::update::UpdateModel;

fn main() {
    init_tracing();
    let data_dir = termoak_client::vault::data_dir();

    // A `termoak://` link opened from outside: to the running instance if
    // there is one, otherwise to the window of this one.
    if let Some(link) = links::from_args(std::env::args()) {
        if links::forward_to_running(&data_dir, &link) {
            return;
        }
        links::deliver(link);
    }

    // System language until the saved choice is read (startup errors).
    i18n::apply(None);

    // 1. Pending update (if applied, the app relaunches right here).
    let updater = update::build(&data_dir);
    if let Some(u) = &updater {
        update::apply_at_startup(u);
    }

    // 2. Tokio runtime for network and disk.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("termoak-io")
        .build()
    {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            eprintln!("Termoak cannot start: could not create the tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    // 3. Local vault.
    let workspace = open_workspace();
    let settings = match &workspace {
        Ok(ws) => Settings::load_blocking(ws, &rt),
        Err(_) => Settings::default(),
    };

    // 4. Interface, in the saved language.
    i18n::apply(settings.language.as_deref());
    links::listen(&data_dir);
    links::register_scheme(&data_dir);
    let application = gpui_platform::application().with_assets(gpui_kit_assets::AllAssets);
    application.on_open_urls(|urls| urls.into_iter().for_each(links::deliver));
    // macOS: the Dock icon clicked with no window on screen (closed or
    // hidden): the window comes back, with its tabs.
    application.on_reopen(windows::reopen);
    application.run(move |cx: &mut App| {
        // Before any window: Windows toasts need the AppUserModelID and
        // Linux shows the name in the notifications.
        cx.set_app_identity(notifications::APP_ID, notifications::APP_NAME);
        gpui_component::init(cx);
        runtime::init(cx, rt);
        theme::init(cx, settings.dark);
        // The terminal keys first: the menu bar shows the shortcuts
        // bound when it is built (in `app::init`).
        terminal::init(cx);
        app::init(cx);

        let options = window_options(cx);
        let opened = match workspace {
            Ok(ws) => cx.open_window(options, move |window, cx| {
                let (prompter, prompts_rx) = DesktopPrompter::new();
                let prompter = Arc::new(prompter);
                let model = cx.new(|cx| AppModel::new(ws, settings, prompter, cx));
                let updates = cx.new(|cx| UpdateModel::new(updater, cx));
                let view = cx.new(|cx| AppView::new(model, updates, Some(prompts_rx), window, cx));
                window.focus(&view.focus_handle(cx), cx);
                cx.new(|cx| Root::new(view, window, cx))
            }),
            Err(error) => cx.open_window(options, move |window, cx| {
                let view = cx.new(|_| StartupError {
                    message: error.message.into(),
                    keychain: error.keychain,
                });
                cx.new(|cx| Root::new(view, window, cx))
            }),
        };
        if let Err(e) = opened {
            eprintln!("could not open the window: {e}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("TERMOAK_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,termoak=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Opens the local vault. Its key is in the system keychain and `keyring`
/// must not be used from the main thread nor inside tokio: it runs in its
/// own thread, with a time limit in case the keychain does not respond.
fn open_workspace() -> Result<Workspace, OpenError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("termoak-vault".into())
        .spawn(move || {
            let _ = tx.send(open_vault());
        })
        .map_err(|e| OpenError::other(e.to_string()))?;
    rx.recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| {
            Err(OpenError {
                message: t!("startup.keychain_timeout").to_string(),
                keychain: true,
            })
        })
}

/// Why the local vault could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenError {
    message: String,
    /// The system keychain refused or failed: allowing access and trying
    /// again may fix it.
    keychain: bool,
}

impl OpenError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            keychain: false,
        }
    }
}

impl From<termoak_client::ClientError> for OpenError {
    fn from(e: termoak_client::ClientError) -> Self {
        match e {
            termoak_client::ClientError::KeychainUnavailable(error) => Self {
                message: t!("startup.keychain_denied", error = error).to_string(),
                keychain: true,
            },
            e => Self::other(e.to_string()),
        }
    }
}

/// Opens the user's workspace (data directory and database) with the vault
/// key from `TERMOAK_VAULT_KEY` or the system keychain. The rules about the
/// keychain (read once, never a new key when it refuses and there is data,
/// the AceitunoakSSH item, `vault.key`) are the core's
/// (`termoak_client::vault::load_key` with the system keychain).
fn open_vault() -> Result<Workspace, OpenError> {
    use termoak_client::vault;
    let dir = vault::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| OpenError::other(e.to_string()))?;
    let key = vault::load_or_create_key(&dir)?;
    Ok(Workspace::open(&dir, key)?)
}

fn window_options(cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(860.), px(540.))),
        app_id: Some(notifications::APP_ID.into()),
        ..TitleBar::window_options()
    }
}

/// Error screen when the local vault could not be opened.
struct StartupError {
    message: SharedString,
    /// The keychain refused or did not answer: say how to fix it.
    keychain: bool,
}

impl Render for StartupError {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(TitleBar::new().child(div().text_sm().child("Termoak")))
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_8()
                    .child(
                        div()
                            .text_xl()
                            .font_semibold()
                            .child(t!("startup.vault_error")),
                    )
                    .child(
                        div()
                            .max_w(px(640.))
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child(self.message.clone()),
                    )
                    .when(self.keychain, |this| {
                        this.child(
                            div()
                                .max_w(px(640.))
                                .text_sm()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child(t!("startup.keychain_hint")),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("quit")
                                    .label(t!("startup.quit"))
                                    .on_click(|_: &ClickEvent, _, cx| cx.quit()),
                            )
                            .child(
                                Button::new("retry")
                                    .primary()
                                    .label(t!("startup.retry"))
                                    // A new process: the keychain is asked again.
                                    .on_click(|_: &ClickEvent, _, cx| cx.restart()),
                            ),
                    ),
            )
    }
}
