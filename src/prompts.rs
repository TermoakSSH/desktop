//! Authentication questions with dialogs: fingerprint of an unknown host,
//! passwords, passphrases and 2FA codes (keyboard-interactive).
//!
//! The SSH engine runs on tokio and asks through [`DesktopPrompter`]; the
//! question travels through a channel to the window, which opens a dialog
//! and sends the answer back through a `oneshot`. Closing the dialog is the
//! same as cancelling.

use std::cell::RefCell;
use std::rc::Rc;

use async_trait::async_trait;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div,
};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme, v_flex};
use termoak_ssh::prompt::{AuthPrompter, Prompt};
use tokio::sync::{mpsc, oneshot};

use crate::ui;

/// Question waiting for an answer.
pub enum PromptRequest {
    HostKey {
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        reply: oneshot::Sender<bool>,
    },
    Keyboard {
        host: String,
        name: String,
        instructions: String,
        prompts: Vec<Prompt>,
        reply: oneshot::Sender<Option<Vec<String>>>,
    },
    Passphrase {
        host: String,
        key_label: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Password {
        host: String,
        user: String,
        reply: oneshot::Sender<Option<String>>,
    },
}

/// [`AuthPrompter`] implementation for the desktop app.
pub struct DesktopPrompter {
    tx: mpsc::UnboundedSender<PromptRequest>,
}

impl DesktopPrompter {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<PromptRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// Sends a question to the interface (server sessions use it too).
    pub fn ask(&self, req: PromptRequest) {
        let _ = self.tx.send(req);
    }
}

#[async_trait]
impl AuthPrompter for DesktopPrompter {
    async fn confirm_host_key(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> bool {
        let (reply, rx) = oneshot::channel();
        self.ask(PromptRequest::HostKey {
            host: host.to_string(),
            port,
            key_type: key_type.to_string(),
            fingerprint: fingerprint.to_string(),
            reply,
        });
        rx.await.unwrap_or(false)
    }

    async fn keyboard_interactive(
        &self,
        host: &str,
        name: &str,
        instructions: &str,
        prompts: &[Prompt],
    ) -> Option<Vec<String>> {
        let (reply, rx) = oneshot::channel();
        self.ask(PromptRequest::Keyboard {
            host: host.to_string(),
            name: name.to_string(),
            instructions: instructions.to_string(),
            prompts: prompts.to_vec(),
            reply,
        });
        rx.await.ok().flatten()
    }

    async fn passphrase(&self, host: &str, key_label: &str) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.ask(PromptRequest::Passphrase {
            host: host.to_string(),
            key_label: key_label.to_string(),
            reply,
        });
        rx.await.ok().flatten()
    }

    async fn password(&self, host: &str, user: &str) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        self.ask(PromptRequest::Password {
            host: host.to_string(),
            user: user.to_string(),
            reply,
        });
        rx.await.ok().flatten()
    }
}

/// Handles the questions in the given window (one after another).
pub fn listen<T: 'static>(
    mut rx: mpsc::UnboundedReceiver<PromptRequest>,
    window: &Window,
    cx: &mut Context<T>,
) {
    cx.spawn_in(window, async move |_, cx| {
        while let Some(req) = rx.recv().await {
            if cx.update(|window, cx| show(req, window, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Single-use answer shared by the buttons of the dialog.
type Reply<T> = Rc<RefCell<Option<oneshot::Sender<T>>>>;

fn reply<T>(tx: oneshot::Sender<T>) -> Reply<T> {
    Rc::new(RefCell::new(Some(tx)))
}

fn send<T>(r: &Reply<T>, value: T) {
    if let Some(tx) = r.borrow_mut().take() {
        let _ = tx.send(value);
    }
}

fn masked_input(
    window: &mut Window,
    cx: &mut App,
    placeholder: impl Into<SharedString>,
) -> Entity<InputState> {
    let placeholder = placeholder.into();
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(placeholder)
    })
}

/// Opens the dialog of a question.
pub fn show(req: PromptRequest, window: &mut Window, cx: &mut App) {
    match req {
        PromptRequest::HostKey {
            host,
            port,
            key_type,
            fingerprint,
            reply: tx,
        } => {
            let r = reply(tx);
            let r_ok = r.clone();
            let host_port: SharedString = format!("{host}:{port}").into();
            let fp: SharedString = fingerprint.into();
            let kt: SharedString = key_type.into();
            ui::open_form_dialog(
                window,
                cx,
                t!("prompts.host_key.title"),
                t!("prompts.host_key.trust"),
                480.,
                move |_, cx| {
                    v_flex()
                        .gap_3()
                        .child(div().child(t!("prompts.host_key.message", host = host_port)))
                        .child(
                            v_flex()
                                .gap_1()
                                .p_3()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(t!("prompts.host_key.fingerprint", key_type = kt)),
                                )
                                .child(
                                    div()
                                        .font_family(ui::mono_family(cx))
                                        .text_sm()
                                        .child(fp.clone()),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("prompts.host_key.hint")),
                        )
                        .into_any_element()
                },
                move |_, _| {
                    send(&r_ok, true);
                    true
                },
            );
            // If the dialog is closed without accepting, `r` is dropped and the connection is refused.
            drop(r);
        }
        PromptRequest::Password {
            host,
            user,
            reply: tx,
        } => {
            let r = reply(tx);
            let input = masked_input(window, cx, t!("prompts.password.placeholder"));
            ui::focus_later(&input, window, cx);
            let label = t!("prompts.password.label", user = user, host = host);
            let input_ok = input.clone();
            ui::open_form_dialog(
                window,
                cx,
                t!("prompts.password.title"),
                t!("common.connect"),
                420.,
                move |_, _| {
                    v_flex()
                        .gap_2()
                        .child(label.clone())
                        .child(Input::new(&input).mask_toggle())
                        .into_any_element()
                },
                move |_, cx| {
                    let v = input_ok.read(cx).value().to_string();
                    send(&r, Some(v));
                    true
                },
            );
        }
        PromptRequest::Passphrase {
            host,
            key_label,
            reply: tx,
        } => {
            let r = reply(tx);
            let input = masked_input(window, cx, t!("prompts.passphrase.placeholder"));
            ui::focus_later(&input, window, cx);
            let label = t!("prompts.passphrase.label", key = key_label, host = host);
            let input_ok = input.clone();
            ui::open_form_dialog(
                window,
                cx,
                t!("prompts.passphrase.title"),
                t!("prompts.passphrase.unlock"),
                440.,
                move |_, _| {
                    v_flex()
                        .gap_2()
                        .child(label.clone())
                        .child(Input::new(&input).mask_toggle())
                        .into_any_element()
                },
                move |_, cx| {
                    let v = input_ok.read(cx).value().to_string();
                    send(&r, Some(v));
                    true
                },
            );
        }
        PromptRequest::Keyboard {
            host,
            name,
            instructions,
            prompts,
            reply: tx,
        } => {
            let r = reply(tx);
            let inputs: Vec<(SharedString, Entity<InputState>)> = prompts
                .iter()
                .map(|p| {
                    let state = cx.new(|cx| InputState::new(window, cx).masked(!p.echo));
                    (SharedString::from(p.text.trim().to_string()), state)
                })
                .collect();
            if let Some((_, first)) = inputs.first() {
                ui::focus_later(&first, window, cx);
            }
            let title = if name.trim().is_empty() {
                t!("prompts.keyboard.title").to_string()
            } else {
                name.trim().to_string()
            };
            let header: SharedString = if instructions.trim().is_empty() {
                t!("prompts.keyboard.header", host = host)
            } else {
                instructions.trim().to_string().into()
            };
            let inputs_ok = inputs.clone();
            ui::open_form_dialog(
                window,
                cx,
                title,
                t!("prompts.keyboard.send"),
                440.,
                move |_, _| {
                    v_flex()
                        .gap_3()
                        .child(header.clone())
                        .children(inputs.iter().map(|(label, state)| {
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().child(label.clone()))
                                .child(Input::new(state))
                        }))
                        .into_any_element()
                },
                move |_, cx| {
                    let answers = inputs_ok
                        .iter()
                        .map(|(_, s)| s.read(cx).value().to_string())
                        .collect();
                    send(&r, Some(answers));
                    true
                },
            );
        }
    }
}
