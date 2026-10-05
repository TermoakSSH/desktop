//! Termoak for desktop: native SSH client (GPUI, no webviews) for Windows,
//! Linux and macOS, in the style of Termius.
//!
//! Startup:
//! 1. The update downloaded in the previous session (if any) is applied and
//!    the app relaunches already updated, before opening any window.
//! 2. The tokio runtime used by the SSH engine and the API is created.
//! 3. The local vault is opened. Its key is in the system keychain, read
//!    once (see `vault_key`); if the keychain refuses and there is already
//!    data, an error screen offers to try again instead of inventing a key.
//! 4. The interface language is chosen and the window is opened.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// Translations in `locales/<lang>.json` (see docs/I18N.md).
rust_i18n::i18n!("locales", fallback = "en");

// First, so that `t!` and `tn!` are available in every module.
#[macro_use]
mod i18n;

mod app;
mod local_ai;
mod menus;
mod panes;
mod prompts;
mod qr;
mod runtime;
mod state;
mod terminal;
mod theme;
mod ui;
mod update;
mod vault_key;
mod views;

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
    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx: &mut App| {
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
                    let view = cx.new(|cx| AppView::new(model, updates, prompts_rx, window, cx));
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
fn open_workspace() -> Result<Workspace, vault_key::OpenError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("termoak-vault".into())
        .spawn(move || {
            let _ = tx.send(vault_key::open_workspace());
        })
        .map_err(|e| vault_key::OpenError {
            message: e.to_string(),
            keychain: false,
        })?;
    rx.recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| {
            Err(vault_key::OpenError {
                message: t!("startup.keychain_timeout").to_string(),
                keychain: true,
            })
        })
}

fn window_options(cx: &mut App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(860.), px(540.))),
        app_id: Some("com.termoak.Termoak".into()),
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
