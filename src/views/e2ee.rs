//! Settings → Security → End-to-end encryption (0.7 phase 5b,
//! docs-next/E2EE-SSO-AUDIT.md §2.3, §2.5 and §10): set up the account's
//! key (the 24-word recovery phrase shown once, confirmed with three of its
//! words, saved or printed; an optional passphrase with a strength hint),
//! its state (fingerprint, unlocked on this computer or not), unlock this
//! computer (passphrase, recovery phrase with live feedback, or another
//! device with the six-digit code), lock, change or remove the passphrase,
//! a new recovery phrase, replace the key (a lost device) and reset it
//! (everything lost, with the vaults that would be lost). Other devices
//! asking for the key are approved here (and from a dialog that opens by
//! itself while the window is in front).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::spinner::Spinner;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::Account;
use termoak_client::e2ee::PairingRequest;
use termoak_core::Id;
use zeroize::Zeroizing;

use crate::e2ee::{self, E2eeBook, RecoveryFeedback, Strength};
use crate::runtime;
use crate::state::{AppModel, ModelEvent, api_error};
use crate::ui::{self, IconName};

fn account_of(model: &Entity<AppModel>, id: Id, cx: &App) -> Option<Arc<Account>> {
    model.read(cx).ws.account(id).filter(|a| a.is_signed_in())
}

/// After something changed the account's E2EE state: read it again and
/// sync (secrets open or lock).
fn changed(model: &Entity<AppModel>, id: Id, sync: bool, cx: &mut App) {
    model.update(cx, |m, cx| {
        m.refresh_e2ee(id, cx);
        if sync {
            m.sync_account(id, cx);
        }
    });
}

fn muted(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

pub struct E2eePanel {
    model: Entity<AppModel>,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl E2eePanel {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let book = E2eeBook::global(cx);
        let subs = vec![
            cx.observe(&book, |_, _, cx| cx.notify()),
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, _, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.refresh(cx);
                }
            }),
        ];
        let this = Self {
            model,
            busy: false,
            _subs: subs,
        };
        this.refresh(cx);
        this
    }

    fn current(&self, cx: &App) -> Option<Id> {
        let m = self.model.read(cx);
        m.current_account
            .filter(|a| m.logged_in() && m.e2ee_supported(*a))
    }

    pub fn refresh(&self, cx: &mut App) {
        if let Some(id) = self.current(cx) {
            self.model.update(cx, |m, cx| m.refresh_e2ee(id, cx));
        }
    }

    fn lock(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = account_of(&self.model, id, cx) else {
            return;
        };
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_lock().await.map_err(api_error) },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(()) => {
                        ui::success(window, cx, t!("e2ee.locked_toast"));
                        changed(&this.model, id, true, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn remove_passphrase(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("e2ee.passphrase.remove_title"),
            t!("e2ee.passphrase.remove_message"),
            t!("e2ee.passphrase.remove"),
            true,
            move |window, cx| {
                let Some(acc) = account_of(&model, id, cx) else {
                    return;
                };
                let model = model.clone();
                let task = runtime::spawn(cx, async move {
                    acc.e2ee_remove_passphrase().await.map_err(api_error)
                });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(_) => {
                                ui::success(window, cx, t!("e2ee.passphrase.removed"));
                                changed(&model, id, false, cx);
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
            },
        );
    }

    fn render_pairings(
        &self,
        id: Id,
        pairings: &[PairingRequest],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.warning)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::Smartphone).text_color(theme.warning))
                    .child(
                        div()
                            .font_semibold()
                            .child(t!("e2ee.pairing.waiting_title")),
                    ),
            )
            .children(pairings.iter().enumerate().map(|(i, p)| {
                let p2 = p.clone();
                let model = self.model.clone();
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .child(p.pairing.device_name.clone()),
                            )
                            .child(muted(
                                t!(
                                    "e2ee.pairing.expires",
                                    left = e2ee::pairing_left(
                                        p.pairing.expires_at,
                                        termoak_core::time::now_ms()
                                    )
                                ),
                                cx,
                            )),
                    )
                    .child(
                        Button::new(("pairing-review", i))
                            .small()
                            .primary()
                            .label(t!("e2ee.pairing.review"))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                open_pairing_approval(model.clone(), id, p2.clone(), window, cx)
                            }),
                    )
            }))
            .into_any_element()
    }
}

impl Render for E2eePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(id) = self.current(cx) else {
            return div().into_any_element();
        };
        let state = self.model.read(cx).e2ee_of(id, cx).unwrap_or_default();
        let theme = cx.theme();
        let title = h_flex()
            .gap_2()
            .items_center()
            .child(ui::icon(IconName::ShieldLock).text_color(theme.primary))
            .child(div().font_semibold().child(t!("e2ee.title")))
            .when(state.set_up(), |this| {
                this.child(if state.unlocked {
                    ui::pill(t!("e2ee.state.unlocked"), theme.success)
                } else {
                    ui::pill(t!("e2ee.state.locked"), theme.warning)
                })
            });
        let mut body = v_flex().gap_3().child(title);
        if !state.loaded {
            return body
                .child(
                    h_flex()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(muted(t!("common.loading"), cx)),
                )
                .into_any_element();
        }
        if let Some(e) = &state.error {
            body = body.child(div().text_sm().text_color(theme.danger).child(e.clone()));
        }
        let model = self.model.clone();
        let Some(key) = state.key.clone() else {
            // Not set up yet.
            return body
                .child(muted(t!("e2ee.intro"), cx))
                .child(muted(t!("e2ee.intro_secrets_only"), cx))
                .child(
                    h_flex().child(
                        Button::new("e2ee-setup")
                            .primary()
                            .icon(ui::icon(IconName::ShieldLock))
                            .label(t!("e2ee.setup.button"))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                open_setup(model.clone(), id, window, cx)
                            }),
                    ),
                )
                .into_any_element();
        };
        body = body
            .child(
                v_flex()
                    .gap_1()
                    .child(muted(
                        if state.unlocked {
                            t!("e2ee.state.unlocked_hint")
                        } else {
                            t!("e2ee.state.locked_hint")
                        },
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("e2ee.fingerprint")),
                            )
                            .child(
                                div()
                                    .font_family(ui::mono_family(cx))
                                    .child(key.fingerprint.clone()),
                            ),
                    )
                    .child(muted(
                        match (state.has_passphrase, state.has_recovery) {
                            (true, true) => t!("e2ee.methods.both"),
                            (false, true) => t!("e2ee.methods.recovery"),
                            (true, false) => t!("e2ee.methods.passphrase"),
                            (false, false) => t!("e2ee.methods.none"),
                        },
                        cx,
                    ))
                    .when(state.locked_secrets > 0 && !state.unlocked, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.warning)
                                .child(tn!("e2ee.locked_secrets", state.locked_secrets)),
                        )
                    })
                    .when(state.waiting > 0, |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.warning)
                                .child(e2ee::waiting_notice(state.waiting).unwrap_or_default()),
                        )
                    }),
            )
            .when(!state.pairings.is_empty() && state.unlocked, |this| {
                this.child(self.render_pairings(id, &state.pairings, cx))
            });
        let has_pass = state.has_passphrase;
        let mut buttons = h_flex().gap_2().flex_wrap();
        if state.unlocked {
            let m1 = model.clone();
            let m2 = model.clone();
            let m3 = model.clone();
            buttons = buttons
                .child(
                    Button::new("e2ee-lock")
                        .icon(ui::icon(IconName::Lock))
                        .label(t!("e2ee.lock"))
                        .loading(self.busy)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.lock(id, window, cx)
                        })),
                )
                .child(
                    Button::new("e2ee-passphrase")
                        .icon(ui::icon(IconName::KeyRound))
                        .label(if has_pass {
                            t!("e2ee.passphrase.change")
                        } else {
                            t!("e2ee.passphrase.set")
                        })
                        .on_click(move |_: &ClickEvent, window, cx| {
                            open_passphrase(m1.clone(), id, window, cx)
                        }),
                )
                .when(has_pass, |this| {
                    this.child(
                        Button::new("e2ee-passphrase-remove")
                            .ghost()
                            .label(t!("e2ee.passphrase.remove"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.remove_passphrase(id, window, cx)
                            })),
                    )
                })
                .child(
                    Button::new("e2ee-new-recovery")
                        .icon(ui::icon(IconName::ScrollText))
                        .label(t!("e2ee.recovery.new"))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            new_recovery(m2.clone(), id, window, cx)
                        }),
                )
                .child(
                    Button::new("e2ee-rotate-key")
                        .icon(ui::icon(IconName::RotateCcwKey))
                        .label(t!("e2ee.rotate.button"))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            open_rotate(m3.clone(), id, window, cx)
                        }),
                );
        } else {
            let m1 = model.clone();
            buttons = buttons.child(
                Button::new("e2ee-unlock")
                    .primary()
                    .icon(ui::icon(IconName::LockOpen))
                    .label(t!("e2ee.unlock.button"))
                    .on_click(move |_: &ClickEvent, window, cx| {
                        open_unlock(m1.clone(), id, window, cx)
                    }),
            );
        }
        let m4 = model.clone();
        buttons = buttons.child(
            Button::new("e2ee-reset")
                .ghost()
                .danger()
                .label(t!("e2ee.reset.button"))
                .on_click(move |_: &ClickEvent, window, cx| open_reset(m4.clone(), id, window, cx)),
        );
        body.child(buttons).into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Set up, new recovery phrase, replace the key: the phrase shown once
// ---------------------------------------------------------------------------

/// Where the phrase dialog is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhraseStep {
    /// Setup and replace: the optional passphrase first.
    Passphrase,
    /// The 24 words.
    Show,
    /// Three of them typed again.
    Confirm,
}

/// What made the phrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhraseKind {
    Setup,
    NewRecovery,
    Rotate,
}

struct PhraseDialog {
    model: Entity<AppModel>,
    account: Id,
    kind: PhraseKind,
    step: PhraseStep,
    phrase: Option<Zeroizing<String>>,
    stored: bool,
    positions: [usize; 3],
    confirm: [Entity<InputState>; 3],
    passphrase: Entity<InputState>,
    passphrase2: Entity<InputState>,
    strength: Strength,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl PhraseDialog {
    fn new(
        model: Entity<AppModel>,
        account: Id,
        kind: PhraseKind,
        phrase: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let masked = |placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            })
        };
        let passphrase = masked(t!("e2ee.passphrase.placeholder"), window, cx);
        let passphrase2 = masked(t!("e2ee.passphrase.repeat"), window, cx);
        let confirm = [
            cx.new(|cx| InputState::new(window, cx)),
            cx.new(|cx| InputState::new(window, cx)),
            cx.new(|cx| InputState::new(window, cx)),
        ];
        let subs = vec![cx.subscribe_in(
            &passphrase,
            window,
            |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.strength = e2ee::passphrase_strength(&input.read(cx).value());
                    cx.notify();
                }
            },
        )];
        let seed = uuid::Uuid::new_v4().as_u128() as u64;
        Self {
            model,
            account,
            kind,
            step: if phrase.is_some() {
                PhraseStep::Show
            } else {
                PhraseStep::Passphrase
            },
            phrase,
            stored: false,
            positions: e2ee::confirm_positions(seed),
            confirm,
            passphrase,
            passphrase2,
            strength: Strength::TooShort,
            busy: false,
            _subs: subs,
        }
    }

    fn title(&self) -> SharedString {
        match self.kind {
            PhraseKind::Setup => t!("e2ee.setup.title"),
            PhraseKind::NewRecovery => t!("e2ee.recovery.new_title"),
            PhraseKind::Rotate => t!("e2ee.rotate.title"),
        }
    }

    /// Creates the key (setup) or the new key (replace) with the optional
    /// passphrase, then shows the phrase.
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let p1 = Zeroizing::new(self.passphrase.read(cx).value().to_string());
        let p2 = Zeroizing::new(self.passphrase2.read(cx).value().to_string());
        if !p1.is_empty() {
            if p1.chars().count() < 8 {
                ui::error(window, cx, t!("error.e2ee_passphrase_too_short"));
                return;
            }
            if *p1 != *p2 {
                ui::error(window, cx, t!("e2ee.passphrase.mismatch"));
                return;
            }
        }
        let Some(acc) = account_of(&self.model, self.account, cx) else {
            return;
        };
        let rotate = self.kind == PhraseKind::Rotate;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                let pass = (!p1.is_empty()).then(|| p1.as_str().to_string());
                let res = if rotate {
                    acc.e2ee_rotate_user_key(pass.as_deref()).await
                } else {
                    acc.e2ee_setup(pass.as_deref()).await
                };
                res.map(|(_, phrase)| phrase).map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(phrase) => {
                        this.phrase = Some(phrase);
                        this.step = PhraseStep::Show;
                        for i in [&this.passphrase, &this.passphrase2] {
                            i.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        changed(&this.model, this.account, true, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn account_label(&self, cx: &App) -> (String, String) {
        let m = self.model.read(cx);
        let a = m.account(self.account);
        (
            a.map(|a| a.info.email.clone()).unwrap_or_default(),
            a.map(|a| a.info.server_url.clone()).unwrap_or_default(),
        )
    }

    fn save(&self, print: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(phrase) = self.phrase.clone() else {
            return;
        };
        let (who, server) = self.account_label(cx);
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let (name, text) = if print {
            (
                "termoak-recovery-phrase.html",
                Zeroizing::new(e2ee::recovery_print_page(&phrase, &who, &server, &date)),
            )
        } else {
            (
                "termoak-recovery-phrase.txt",
                Zeroizing::new(e2ee::recovery_document(&phrase, &who, &server, &date)),
            )
        };
        let dir = directories::UserDirs::new()
            .and_then(|d| d.document_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let res = std::fs::write(&path, text.as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            let _ = cx.update(|window, cx| match res {
                Ok(()) => {
                    if print && let Ok(url) = url::Url::from_file_path(&path) {
                        cx.open_url(url.as_str());
                    }
                    ui::success(
                        window,
                        cx,
                        t!("e2ee.recovery.saved", path = path.display().to_string()),
                    );
                }
                Err(e) => ui::error(window, cx, e.to_string()),
            });
        })
        .detach();
    }

    fn check_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(phrase) = &self.phrase else {
            return;
        };
        let typed: Vec<String> = self
            .confirm
            .iter()
            .map(|i| i.read(cx).value().to_string())
            .collect();
        if e2ee::confirm_matches(phrase, &self.positions, &typed) {
            ui::success(
                window,
                cx,
                match self.kind {
                    PhraseKind::Setup => t!("e2ee.setup.done"),
                    PhraseKind::NewRecovery => t!("e2ee.recovery.new_done"),
                    PhraseKind::Rotate => t!("e2ee.rotate.done"),
                },
            );
            self.phrase = None;
            window.close_dialog(cx);
        } else {
            ui::error(window, cx, t!("e2ee.confirm.wrong"));
        }
    }

    /// Closing before the confirmation: the phrase is not shown again.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phrase.is_none() || self.step == PhraseStep::Passphrase {
            window.close_dialog(cx);
            return;
        }
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("e2ee.confirm.close_title"),
            t!("e2ee.confirm.close_message"),
            t!("e2ee.confirm.close_anyway"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, _| v.phrase = None);
                }
                window.close_dialog(cx);
            },
        );
    }

    fn render_passphrase(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let color = match self.strength {
            Strength::TooShort | Strength::Weak => theme.danger,
            Strength::Fair => theme.warning,
            _ => theme.success,
        };
        let typed = !self.passphrase.read(cx).value().is_empty();
        v_flex()
            .gap_3()
            .child(muted(
                match self.kind {
                    PhraseKind::Rotate => t!("e2ee.rotate.message"),
                    _ => t!("e2ee.setup.message"),
                },
                cx,
            ))
            .child(ui::field(
                t!("e2ee.passphrase.optional"),
                Input::new(&self.passphrase),
                cx,
            ))
            .child(Input::new(&self.passphrase2))
            .when(typed, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(color)
                        .child(e2ee::strength_text(self.strength)),
                )
            })
            .child(muted(t!("e2ee.passphrase.hint"), cx))
            .into_any_element()
    }

    fn render_show(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let phrase = self.phrase.clone().unwrap_or_default();
        let words = e2ee::numbered_words(&phrase);
        let mono = ui::mono_family(cx);
        let p2 = phrase.clone();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("e2ee.recovery.show_message")),
            )
            .child(
                div()
                    .grid()
                    .grid_cols(4)
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .border_1()
                    .border_color(theme.border)
                    .children(words.into_iter().map(|(n, w)| {
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(22.))
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{n}")),
                            )
                            .child(div().font_family(mono.clone()).child(w))
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("phrase-copy")
                            .small()
                            .icon(ui::icon(IconName::Copy))
                            .label(t!("common.copy"))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(p2.to_string()));
                                ui::success(window, cx, t!("e2ee.recovery.copied"));
                            }),
                    )
                    .child(
                        Button::new("phrase-save")
                            .small()
                            .icon(ui::icon(IconName::Download))
                            .label(t!("e2ee.recovery.save"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("phrase-print")
                            .small()
                            .icon(ui::icon(IconName::Printer))
                            .label(t!("e2ee.recovery.print"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(true, window, cx)
                            })),
                    ),
            )
            .child(muted(t!("e2ee.recovery.keep_hint"), cx))
            .child(
                Checkbox::new("phrase-stored")
                    .label(t!("e2ee.recovery.stored"))
                    .checked(self.stored)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.stored = *v;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_3()
            .child(muted(t!("e2ee.confirm.message"), cx))
            .children(self.positions.iter().zip(&self.confirm).map(|(p, input)| {
                ui::field(t!("e2ee.confirm.word", n = p + 1), Input::new(input), cx)
            }))
            .into_any_element()
    }
}

impl Render for PhraseDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.step {
            PhraseStep::Passphrase => self.render_passphrase(cx),
            PhraseStep::Show => self.render_show(cx),
            PhraseStep::Confirm => self.render_confirm(cx),
        };
        let footer = h_flex()
            .w_full()
            .justify_end()
            .gap_2()
            .child(
                Button::new("phrase-cancel")
                    .label(if self.step == PhraseStep::Passphrase {
                        t!("common.cancel")
                    } else {
                        t!("common.close")
                    })
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.close(window, cx)),
                    ),
            )
            .map(|this| match self.step {
                PhraseStep::Passphrase => this.child(
                    Button::new("phrase-create")
                        .primary()
                        .label(match self.kind {
                            PhraseKind::Rotate => t!("e2ee.rotate.confirm"),
                            _ => t!("e2ee.setup.create"),
                        })
                        .loading(self.busy)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.create(window, cx)),
                        ),
                ),
                PhraseStep::Show => this.child(
                    Button::new("phrase-next")
                        .primary()
                        .label(t!("common.next"))
                        .disabled(!self.stored)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.step = PhraseStep::Confirm;
                            ui::focus_later(&this.confirm[0], window, cx);
                            cx.notify();
                        })),
                ),
                PhraseStep::Confirm => this
                    .child(
                        Button::new("phrase-back")
                            .label(t!("e2ee.confirm.back"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.step = PhraseStep::Show;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("phrase-done")
                            .primary()
                            .label(t!("e2ee.confirm.done"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.check_confirm(window, cx)
                            })),
                    ),
            });
        v_flex().gap_4().child(body).child(footer)
    }
}

fn open_phrase_dialog(
    model: Entity<AppModel>,
    account: Id,
    kind: PhraseKind,
    phrase: Option<Zeroizing<String>>,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| PhraseDialog::new(model, account, kind, phrase, window, cx));
    let title = view.read(cx).title();
    let focus = view.read(cx).passphrase.clone();
    window.open_dialog(cx, move |d, _, _| {
        d.title(title.clone())
            .w(px(600.))
            .overlay_closable(false)
            .child(view.clone())
    });
    ui::focus_later(&focus, window, cx);
}

/// Set up: the optional passphrase, the key, the phrase shown once.
pub fn open_setup(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    open_phrase_dialog(model, account, PhraseKind::Setup, None, window, cx);
}

/// Replace the key (a lost device).
pub fn open_rotate(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    open_phrase_dialog(model, account, PhraseKind::Rotate, None, window, cx);
}

/// A new recovery phrase (the old one stops working).
pub fn new_recovery(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    let m = model.clone();
    ui::confirm(
        window,
        cx,
        t!("e2ee.recovery.new_title"),
        t!("e2ee.recovery.new_message"),
        t!("e2ee.recovery.new_confirm"),
        false,
        move |window, cx| {
            let Some(acc) = account_of(&m, account, cx) else {
                return;
            };
            let model = m.clone();
            let task = runtime::spawn(cx, async move {
                acc.e2ee_new_recovery_key().await.map_err(api_error)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(phrase) => {
                            changed(&model, account, false, cx);
                            open_phrase_dialog(
                                model.clone(),
                                account,
                                PhraseKind::NewRecovery,
                                Some(phrase),
                                window,
                                cx,
                            );
                        }
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
        },
    );
}

// ---------------------------------------------------------------------------
// Passphrase
// ---------------------------------------------------------------------------

pub fn open_passphrase(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    let p1 = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(t!("e2ee.passphrase.placeholder"))
    });
    let p2 = cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(t!("e2ee.passphrase.repeat"))
    });
    let (a, b) = (p1.clone(), p2.clone());
    ui::open_form_dialog(
        window,
        cx,
        t!("e2ee.passphrase.title"),
        t!("common.save"),
        480.,
        move |_, cx| {
            let strength = e2ee::passphrase_strength(&a.read(cx).value());
            v_flex()
                .gap_3()
                .child(muted(t!("e2ee.passphrase.hint"), cx))
                .child(Input::new(&a))
                .child(Input::new(&b))
                .child(muted(e2ee::strength_text(strength), cx))
                .into_any_element()
        },
        move |window, cx| {
            let v1 = Zeroizing::new(p1.read(cx).value().to_string());
            let v2 = Zeroizing::new(p2.read(cx).value().to_string());
            if v1.chars().count() < 8 {
                ui::error(window, cx, t!("error.e2ee_passphrase_too_short"));
                return false;
            }
            if *v1 != *v2 {
                ui::error(window, cx, t!("e2ee.passphrase.mismatch"));
                return false;
            }
            let Some(acc) = account_of(&model, account, cx) else {
                return true;
            };
            let model = model.clone();
            let task = runtime::spawn(cx, async move {
                acc.e2ee_set_passphrase(&v1).await.map_err(api_error)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(()) => {
                            ui::success(window, cx, t!("e2ee.passphrase.saved"));
                            changed(&model, account, false, cx);
                        }
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
            true
        },
    );
}

// ---------------------------------------------------------------------------
// Unlock
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnlockWay {
    Passphrase,
    Recovery,
    Device,
}

struct UnlockDialog {
    model: Entity<AppModel>,
    account: Id,
    way: UnlockWay,
    has_passphrase: bool,
    passphrase: Entity<InputState>,
    recovery: Entity<TextareaState>,
    feedback: RecoveryFeedback,
    busy: bool,
    /// Pairing in progress: its id, code and the private pairing key.
    pairing: Option<(Id, String, i64, Zeroizing<[u8; 32]>)>,
    pairing_error: Option<String>,
    poll: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl UnlockDialog {
    fn new(
        model: Entity<AppModel>,
        account: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let has_passphrase = model
            .read(cx)
            .e2ee_of(account, cx)
            .is_some_and(|s| s.has_passphrase);
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("e2ee.passphrase.placeholder"))
        });
        let recovery = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 6)
                .placeholder(t!("e2ee.unlock.recovery_placeholder"))
        });
        let subs = vec![
            cx.subscribe_in(&recovery, window, |this, input, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.feedback = e2ee::recovery_feedback(&input.read(cx).value());
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &passphrase,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.unlock(window, cx);
                    }
                },
            ),
            // An `e2ee` event (the pairing decided) checks at once.
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::Server(v) = ev
                    && e2ee::is_pairing_event(v)
                    && this.pairing.is_some()
                {
                    this.check_pairing(window, cx);
                }
            }),
        ];
        Self {
            model,
            account,
            way: if has_passphrase {
                UnlockWay::Passphrase
            } else {
                UnlockWay::Recovery
            },
            has_passphrase,
            passphrase,
            recovery,
            feedback: RecoveryFeedback::Typing(0),
            busy: false,
            pairing: None,
            pairing_error: None,
            poll: None,
            _subs: subs,
        }
    }

    fn unlocked(&mut self, opened: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.poll = None;
        self.pairing = None;
        ui::success(window, cx, tn!("e2ee.unlock.done", opened));
        changed(&self.model, self.account, true, cx);
        window.close_dialog(cx);
    }

    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(acc) = account_of(&self.model, self.account, cx) else {
            return;
        };
        let way = self.way;
        let secret = Zeroizing::new(match way {
            UnlockWay::Passphrase => self.passphrase.read(cx).value().to_string(),
            UnlockWay::Recovery => self.recovery.read(cx).value().to_string(),
            UnlockWay::Device => return,
        });
        if secret.trim().is_empty() {
            return;
        }
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                match way {
                    UnlockWay::Passphrase => acc.e2ee_unlock_passphrase(&secret).await,
                    _ => acc.e2ee_unlock_recovery(&secret).await,
                }
                .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(n) => this.unlocked(n, window, cx),
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn start_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = account_of(&self.model, self.account, cx) else {
            return;
        };
        self.busy = true;
        self.pairing_error = None;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_pairing_start().await.map_err(api_error) },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok((start, key)) => {
                        this.pairing = Some((start.id, start.code, start.expires_at, key));
                        this.poll_pairing(window, cx);
                    }
                    Err(e) => this.pairing_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    /// Checks every 4 s (and at each `e2ee` event) until decided.
    fn poll_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        if this.pairing.is_none() {
                            return false;
                        }
                        this.check_pairing(window, cx);
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    fn check_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, _, _, key)) = self.pairing.clone() else {
            return;
        };
        let Some(acc) = account_of(&self.model, self.account, cx) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_pairing_finish(id, &key).await.map_err(api_error) },
            move |this, res, window, cx| {
                if this.pairing.as_ref().map(|p| p.0) != Some(id) {
                    return;
                }
                match res {
                    Ok(Some(n)) => this.unlocked(n, window, cx),
                    Ok(None) => {}
                    Err(e) => {
                        this.pairing = None;
                        this.poll = None;
                        this.pairing_error = Some(e);
                    }
                }
                cx.notify();
            },
        );
    }

    fn render_device(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mut col = v_flex()
            .gap_3()
            .child(muted(t!("e2ee.unlock.device_message"), cx));
        if let Some(e) = &self.pairing_error {
            col = col.child(div().text_sm().text_color(theme.danger).child(e.clone()));
        }
        match &self.pairing {
            Some((_, code, expires, _)) => col
                .child(
                    v_flex()
                        .items_center()
                        .gap_1()
                        .p_4()
                        .rounded(theme.radius)
                        .bg(theme.secondary)
                        .child(muted(t!("e2ee.unlock.device_code"), cx))
                        .child(
                            div()
                                .text_3xl()
                                .font_bold()
                                .font_family(ui::mono_family(cx))
                                .child(format!("{} {}", &code[..3], &code[3..])),
                        )
                        .child(muted(
                            t!(
                                "e2ee.pairing.expires",
                                left = e2ee::pairing_left(*expires, termoak_core::time::now_ms())
                            ),
                            cx,
                        )),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(muted(t!("e2ee.unlock.device_waiting"), cx)),
                )
                .into_any_element(),
            None => col
                .child(
                    h_flex().child(
                        Button::new("pair-start")
                            .primary()
                            .icon(ui::icon(IconName::Smartphone))
                            .label(t!("e2ee.unlock.device_start"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.start_pairing(window, cx)
                            })),
                    ),
                )
                .into_any_element(),
        }
    }
}

impl Render for UnlockDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut ways = Vec::new();
        if self.has_passphrase {
            ways.push(UnlockWay::Passphrase);
        }
        ways.push(UnlockWay::Recovery);
        ways.push(UnlockWay::Device);
        let ix = ways.iter().position(|w| *w == self.way).unwrap_or(0);
        let ways2 = ways.clone();
        let tabs = TabBar::new("unlock-ways")
            .segmented()
            .selected_index(ix)
            .children(ways.iter().map(|w| {
                Tab::new().label(match w {
                    UnlockWay::Passphrase => t!("e2ee.unlock.way_passphrase"),
                    UnlockWay::Recovery => t!("e2ee.unlock.way_recovery"),
                    UnlockWay::Device => t!("e2ee.unlock.way_device"),
                })
            }))
            .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                this.way = ways2[*ix];
                match this.way {
                    UnlockWay::Passphrase => ui::focus_later(&this.passphrase, window, cx),
                    UnlockWay::Recovery => ui::focus_later(&this.recovery, window, cx),
                    UnlockWay::Device => {}
                }
                cx.notify();
            }));
        let theme = cx.theme();
        let body = match self.way {
            UnlockWay::Passphrase => v_flex()
                .gap_3()
                .child(muted(t!("e2ee.unlock.passphrase_message"), cx))
                .child(Input::new(&self.passphrase))
                .into_any_element(),
            UnlockWay::Recovery => {
                let (text, color) = match &self.feedback {
                    RecoveryFeedback::Valid => {
                        (t!("e2ee.unlock.recovery_valid").to_string(), theme.success)
                    }
                    RecoveryFeedback::Typing(n) => (
                        t!("e2ee.unlock.recovery_typing", count = n).to_string(),
                        theme.muted_foreground,
                    ),
                    RecoveryFeedback::Problem(p) => (p.clone(), theme.danger),
                };
                v_flex()
                    .gap_3()
                    .child(muted(t!("e2ee.unlock.recovery_message"), cx))
                    .child(Textarea::new(&self.recovery))
                    .child(div().text_xs().text_color(color).child(text))
                    .into_any_element()
            }
            UnlockWay::Device => self.render_device(cx),
        };
        let can_unlock = match self.way {
            UnlockWay::Passphrase => true,
            UnlockWay::Recovery => self.feedback == RecoveryFeedback::Valid,
            UnlockWay::Device => false,
        };
        v_flex().gap_4().child(tabs).child(body).child(
            h_flex()
                .w_full()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("unlock-cancel")
                        .label(t!("common.cancel"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.pairing = None;
                            this.poll = None;
                            window.close_dialog(cx);
                        })),
                )
                .when(self.way != UnlockWay::Device, |this| {
                    this.child(
                        Button::new("unlock-ok")
                            .primary()
                            .icon(ui::icon(IconName::LockOpen))
                            .label(t!("e2ee.unlock.button"))
                            .loading(self.busy)
                            .disabled(!can_unlock)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.unlock(window, cx)
                            })),
                    )
                }),
        )
    }
}

/// Unlock this computer.
pub fn open_unlock(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| UnlockDialog::new(model, account, window, cx));
    let focus = view.read(cx).passphrase.clone();
    let recovery = view.read(cx).recovery.clone();
    let has_pass = view.read(cx).has_passphrase;
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("e2ee.unlock.title"))
            .w(px(560.))
            .overlay_closable(false)
            .child(view.clone())
    });
    if has_pass {
        ui::focus_later(&focus, window, cx);
    } else {
        ui::focus_later(&recovery, window, cx);
    }
}

// ---------------------------------------------------------------------------
// Pairing approval (on an unlocked computer)
// ---------------------------------------------------------------------------

pub fn open_pairing_approval(
    model: Entity<AppModel>,
    account: Id,
    p: PairingRequest,
    window: &mut Window,
    cx: &mut App,
) {
    let code = p.code.clone();
    let device = p.pairing.device_name.clone();
    let expires = p.pairing.expires_at;
    let id = p.pairing.id;
    let decide = Arc::new(move |approve: bool, window: &mut Window, cx: &mut App| {
        window.close_dialog(cx);
        let Some(acc) = account_of(&model, account, cx) else {
            return;
        };
        let model = model.clone();
        let task = runtime::spawn(cx, async move {
            acc.e2ee_pairing_decide(id, approve)
                .await
                .map_err(api_error)
        });
        window
            .spawn(cx, async move |cx| {
                let res = task.await;
                let _ = cx.update(|window, cx| match res {
                    Ok(()) => {
                        ui::success(
                            window,
                            cx,
                            if approve {
                                t!("e2ee.pairing.approved")
                            } else {
                                t!("e2ee.pairing.denied")
                            },
                        );
                        changed(&model, account, false, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                });
            })
            .detach();
    });
    window.open_dialog(cx, move |d, _, cx| {
        let (deny, approve) = (decide.clone(), decide.clone());
        let theme = cx.theme();
        d.title(t!("e2ee.pairing.title"))
            .w(px(460.))
            .overlay_closable(false)
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .child(t!("e2ee.pairing.message", device = device.clone())),
                    )
                    .child(
                        v_flex()
                            .items_center()
                            .p_4()
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .child(
                                div()
                                    .text_3xl()
                                    .font_bold()
                                    .font_family(ui::mono_family(cx))
                                    .child(format!("{} {}", &code[..3], &code[3..])),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                                "e2ee.pairing.expires",
                                left = e2ee::pairing_left(expires, termoak_core::time::now_ms())
                            ))),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.warning)
                            .child(t!("e2ee.pairing.compare")),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("pairing-deny")
                            .danger()
                            .label(t!("e2ee.pairing.deny"))
                            .on_click(move |_, window, cx| deny(false, window, cx)),
                    )
                    .child(
                        Button::new("pairing-approve")
                            .primary()
                            .label(t!("e2ee.pairing.approve"))
                            .on_click(move |_, window, cx| approve(true, window, cx)),
                    ),
            )
    });
}

// ---------------------------------------------------------------------------
// Reset (everything lost)
// ---------------------------------------------------------------------------

pub fn open_reset(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    let Some(acc) = account_of(&model, account, cx) else {
        return;
    };
    let task = runtime::spawn(
        cx,
        async move { acc.e2ee_reset(false).await.map_err(api_error) },
    );
    window
        .spawn(cx, async move |cx| {
            let res = task.await;
            let _ = cx.update(|window, cx| match res {
                Ok(orphaned) => reset_dialog(model.clone(), account, orphaned, window, cx),
                Err(e) => ui::error(window, cx, e),
            });
        })
        .detach();
}

fn reset_dialog(
    model: Entity<AppModel>,
    account: Id,
    orphaned: Vec<Id>,
    window: &mut Window,
    cx: &mut App,
) {
    let email = model
        .read(cx)
        .account(account)
        .map(|a| a.info.email.clone())
        .unwrap_or_default();
    let names: Vec<String> = orphaned
        .iter()
        .map(|v| {
            model
                .read(cx)
                .vault_entry(account, *v)
                .map(crate::accounts::VaultEntry::label)
                .unwrap_or_else(|| v.to_string())
        })
        .collect();
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(email.clone()));
    let i2 = input.clone();
    ui::open_form_dialog(
        window,
        cx,
        t!("e2ee.reset.title"),
        t!("e2ee.reset.confirm"),
        520.,
        move |_, cx| {
            let theme = cx.theme();
            v_flex()
                .gap_3()
                .child(div().text_sm().child(t!("e2ee.reset.message")))
                .child(if names.is_empty() {
                    muted(t!("e2ee.reset.no_vaults"), cx)
                } else {
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(tn!("e2ee.reset.vaults_lost", names.len())),
                        )
                        .children(
                            names
                                .iter()
                                .map(|n| div().text_sm().pl_3().child(format!("• {n}"))),
                        )
                })
                .child(ui::field(
                    t!("e2ee.reset.type_email", email = email.clone()),
                    Input::new(&input),
                    cx,
                ))
                .into_any_element()
        },
        move |window, cx| {
            let typed = i2.read(cx).value().trim().to_lowercase();
            let want = model
                .read(cx)
                .account(account)
                .map(|a| a.info.email.to_lowercase())
                .unwrap_or_default();
            if typed != want {
                ui::error(window, cx, t!("e2ee.reset.mismatch"));
                return false;
            }
            let Some(acc) = account_of(&model, account, cx) else {
                return true;
            };
            let model = model.clone();
            let task = runtime::spawn(
                cx,
                async move { acc.e2ee_reset(true).await.map_err(api_error) },
            );
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(_) => {
                            ui::success(window, cx, t!("e2ee.reset.done"));
                            changed(&model, account, true, cx);
                        }
                        Err(e) => ui::error(window, cx, e),
                    });
                })
                .detach();
            true
        },
    );
}
