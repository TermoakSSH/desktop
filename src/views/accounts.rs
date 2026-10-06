//! Settings → Accounts: every account signed in on this device (one per
//! server and user) with its status, server, vaults and last sync. From here
//! an account is synced, used, signed in again, verified, signed out (its
//! local data is deleted; unsynced changes are asked about first: "Sync now
//! / Discard") or removed. New vaults are created here too.

use gpui::{
    ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::AccountStatus;
use termoak_core::Id;

use super::add_account::{self, Start};
use super::vaults;
use crate::accounts::{self as vm, AccountRow, ViewMode};
use crate::runtime;
use crate::state::{AccountModel, AppModel};
use crate::theme;
use crate::ui::{self, IconName};

pub struct AccountsView {
    model: Entity<AppModel>,
    /// Accounts being signed out (to show the button busy).
    busy: Vec<Id>,
    _subs: Vec<Subscription>,
}

/// Avatar of an account: its initial on its color.
pub fn avatar(row: &AccountRow, size: f32) -> gpui::Div {
    let color = row
        .color
        .as_deref()
        .and_then(theme::parse_color)
        .unwrap_or_else(|| theme::color_for(&row.email));
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .font_semibold()
        .text_size(px(size * 0.5))
        .child(row.initial.clone())
}

impl AccountsView {
    pub fn new(model: Entity<AppModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        Self {
            model,
            busy: Vec::new(),
            _subs: subs,
        }
    }

    /// Signs out of an account; with unsynced changes it asks first.
    fn sign_out(&mut self, id: Id, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.contains(&id) {
            return;
        }
        self.busy.push(id);
        cx.notify();
        let task = self
            .model
            .update(cx, |m, cx| m.sign_out_account(id, discard, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy.retain(|b| *b != id);
                match res {
                    Ok(report) if report.signed_out => {}
                    Ok(report) => this.ask_unsynced(id, report.unsynced, window, cx),
                    Err(e) => ui::error(window, cx, t!("accounts.sign_out_failed", error = e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "N changes are not uploaded yet": sync now (then sign out) or discard.
    fn ask_unsynced(&mut self, id: Id, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let (w1, w2) = (weak.clone(), weak);
        window.open_dialog(cx, move |dialog, _, _| {
            let (w1, w2) = (w1.clone(), w2.clone());
            dialog
                .title(t!("accounts.unsynced_title"))
                .w(px(440.))
                .child(div().child(tn!("accounts.unsynced_message", n)))
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("unsynced-cancel")
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("unsynced-discard")
                                .danger()
                                .label(t!("accounts.discard_sign_out"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    if let Some(v) = w1.upgrade() {
                                        v.update(cx, |v, cx| v.sign_out(id, true, window, cx));
                                    }
                                }),
                        )
                        .child(
                            Button::new("unsynced-sync")
                                .primary()
                                .icon(ui::icon(IconName::RefreshCw))
                                .label(t!("accounts.sync_sign_out"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    if let Some(v) = w2.upgrade() {
                                        v.update(cx, |v, cx| v.sync_then_sign_out(id, window, cx));
                                    }
                                }),
                        ),
                )
        });
    }

    /// Uploads the pending changes, then signs out (asking again if some
    /// could not be uploaded).
    fn sync_then_sign_out(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.model.read(cx).ws.account(id) else {
            return;
        };
        self.busy.push(id);
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.sync_once().await.map_err(crate::state::api_error) },
            move |this, res, window, cx| {
                this.busy.retain(|b| *b != id);
                match res {
                    Ok(_) => this.sign_out(id, false, window, cx),
                    Err(e) => ui::error(window, cx, t!("accounts.sync_failed", error = e)),
                }
            },
        );
    }

    fn confirm_sign_out(&mut self, a: &AccountModel, window: &mut Window, cx: &mut Context<Self>) {
        let id = a.id();
        let email = a.info.email.clone();
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("accounts.sign_out_title"),
            t!("accounts.sign_out_message", email = email),
            t!("accounts.sign_out"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| v.sign_out(id, false, window, cx));
                }
            },
        );
    }

    /// Removes an account that is not signed in (its data goes too).
    fn confirm_remove(&mut self, a: &AccountModel, window: &mut Window, cx: &mut Context<Self>) {
        let id = a.id();
        let unsynced = a.unsynced;
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("accounts.remove_title"),
            if unsynced > 0 {
                tn!(
                    "accounts.remove_message_unsynced",
                    unsynced,
                    email = a.info.email.clone()
                )
            } else {
                t!("accounts.remove_message", email = a.info.email.clone())
            },
            t!("accounts.remove"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| v.sign_out(id, true, window, cx));
                }
            },
        );
    }

    fn confirm_remove_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("accounts.remove_all_title"),
            t!("accounts.remove_all_message"),
            t!("accounts.remove_all"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.sign_out_all(cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
    }

    fn render_account(
        &self,
        ix: usize,
        a: &AccountModel,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let row = AccountRow::new(&a.info);
        let id = a.id();
        let m = self.model.read(cx);
        let current = m.current_account == Some(id);
        let viewing = m.view == ViewMode::Account(id);
        let status = a.info.status;
        let active = a.active();
        let busy = self.busy.contains(&id);
        let (status_color, status_text) = match status {
            AccountStatus::Active if a.signed_in => (
                if a.events_online {
                    theme.success
                } else {
                    theme.warning
                },
                if a.events_online {
                    t!("settings.account.online").to_string()
                } else {
                    vm::status_text(status, true)
                },
            ),
            AccountStatus::Unverified => (theme.warning, vm::status_text(status, false)),
            _ => (theme.danger, vm::status_text(status, false)),
        };
        let last_sync = match (&a.last_sync, a.info.last_sync_at) {
            (Some(Err(e)), _) => (
                t!("settings.account.sync_error", error = e.clone()),
                theme.danger,
            ),
            (_, Some(at)) => (
                t!("accounts.last_sync", time = ui::format_ms(at)),
                theme.muted_foreground,
            ),
            _ => (t!("settings.account.never_synced"), theme.muted_foreground),
        };
        let model = self.model.clone();
        let server_line = format!(
            "{}{}",
            a.info.server_host(),
            if a.info.official {
                format!(" · {}", t!("accounts.official"))
            } else {
                String::new()
            }
        );
        let vault_list = a.vaults.clone();
        let vaults_supported = a.vaults_supported();
        let (m1, m2, m3, m4) = (model.clone(), model.clone(), model.clone(), model.clone());
        let info = a.info.clone();
        let a2 = a.clone();
        let a3 = a.clone();
        ui::card(cx)
            .p_4()
            .gap_3()
            .w_full()
            .max_w(px(760.))
            .when(current, |this| this.border_color(theme.primary))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(avatar(&row, 36.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .when(!a.info.name.is_empty(), |this| {
                                        this.child(div().font_semibold().child(a.info.name.clone()))
                                    })
                                    .child(div().font_medium().child(a.info.email.clone()))
                                    .child(ui::pill(status_text, status_color))
                                    .when(current, |this| {
                                        this.child(ui::pill(t!("accounts.current"), theme.primary))
                                    }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(server_line),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(last_sync.1)
                                    .child(last_sync.0)
                                    .when(a.unsynced > 0, |this| {
                                        this.child(format!(
                                            " · {}",
                                            tn!("accounts.unsynced", a.unsynced)
                                        ))
                                    }),
                            ),
                    ),
            )
            // Vaults of the account.
            .when(vaults_supported && !vault_list.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_semibold()
                                .text_color(theme.muted_foreground)
                                .child(t!("accounts.vaults").to_uppercase()),
                        )
                        .child(h_flex().gap_1().flex_wrap().children(
                            vault_list.into_iter().enumerate().map(|(i, v)| {
                                let model = model.clone();
                                let vid = v.id();
                                Button::new(("account-vault", ix * 1000 + i))
                                    .xsmall()
                                    .ghost()
                                    .child(vaults::chip(&v, cx))
                                    .tooltip(vaults::counts_text(&v.vault))
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        vaults::open_manage(model.clone(), id, vid, window, cx)
                                    })
                            }),
                        )),
                )
            })
            .when(!vaults_supported && active, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("accounts.no_vaults")),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .when(active, |this| {
                        this.child(
                            Button::new(("account-sync", ix))
                                .small()
                                .icon(ui::icon(IconName::RefreshCw))
                                .label(t!("settings.account.sync_now"))
                                .loading(a.syncing)
                                .on_click(move |_: &ClickEvent, _, cx| {
                                    m1.update(cx, |m, cx| m.sync_account(id, cx))
                                }),
                        )
                    })
                    .when(!viewing, |this| {
                        this.child(
                            Button::new(("account-use", ix))
                                .small()
                                .icon(ui::icon(IconName::ArrowRightLeft))
                                .label(t!("accounts.use"))
                                .on_click(move |_: &ClickEvent, _, cx| {
                                    m2.update(cx, |m, cx| m.set_view(ViewMode::Account(id), cx))
                                }),
                        )
                    })
                    .when(active && vaults_supported, |this| {
                        this.child(
                            Button::new(("account-new-vault", ix))
                                .small()
                                .icon(ui::icon(IconName::Plus))
                                .label(t!("vaults.new"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    vaults::open_create(m3.clone(), id, None, window, cx)
                                }),
                        )
                    })
                    .when(status == AccountStatus::Unverified, |this| {
                        let model = model.clone();
                        this.child(
                            Button::new(("account-verify", ix))
                                .small()
                                .primary()
                                .icon(ui::icon(IconName::Mail))
                                .label(t!("accounts.enter_code"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    add_account::open(model.clone(), Start::Verify(id), window, cx)
                                }),
                        )
                    })
                    .when(
                        !a.signed_in && status != AccountStatus::Unverified,
                        |this| {
                            let info = info.clone();
                            this.child(
                                Button::new(("account-sign-in", ix))
                                    .small()
                                    .primary()
                                    .icon(ui::icon(IconName::LogIn))
                                    .label(t!("accounts.sign_in_again"))
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        add_account::open(
                                            m4.clone(),
                                            Start::SignInAgain {
                                                url: info.server_url.clone(),
                                                email: info.email.clone(),
                                            },
                                            window,
                                            cx,
                                        )
                                    }),
                            )
                        },
                    )
                    .child(div().flex_1())
                    .map(|this| {
                        if a.signed_in && status == AccountStatus::Active {
                            this.child(
                                Button::new(("account-sign-out", ix))
                                    .small()
                                    .icon(ui::icon(IconName::LogOut))
                                    .label(t!("accounts.sign_out"))
                                    .loading(busy)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.confirm_sign_out(&a2, window, cx)
                                        },
                                    )),
                            )
                        } else {
                            this.child(
                                Button::new(("account-remove", ix))
                                    .small()
                                    .icon(ui::icon(IconName::Trash))
                                    .label(t!("accounts.remove"))
                                    .loading(busy)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.confirm_remove(&a3, window, cx)
                                        },
                                    )),
                            )
                        }
                    }),
            )
            .into_any_element()
    }
}

impl Render for AccountsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accounts = self.model.read(cx).accounts.clone();
        let rows: Vec<gpui::AnyElement> = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| self.render_account(i, a, cx))
            .collect();
        let theme = cx.theme();
        let empty = accounts.is_empty();
        v_flex().size_full().overflow_y_scrollbar().child(
            v_flex()
                .p_6()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .max_w(px(760.))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("accounts.intro")),
                        )
                        .child(
                            Button::new("accounts-add")
                                .primary()
                                .icon(ui::icon(IconName::UserPlus))
                                .label(t!("accounts.add"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    add_account::open(this.model.clone(), Start::Choose, window, cx)
                                })),
                        ),
                )
                .when(empty, |this| {
                    this.child(ui::empty_state(
                        IconName::Users,
                        t!("accounts.empty"),
                        t!("accounts.empty_hint"),
                        cx,
                    ))
                })
                .children(rows)
                .when(accounts.len() > 1, |this| {
                    this.child(
                        h_flex().max_w(px(760.)).justify_end().child(
                            Button::new("accounts-remove-all")
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .label(t!("accounts.remove_all"))
                                .disabled(!self.busy.is_empty())
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.confirm_remove_all(window, cx)
                                })),
                        ),
                    )
                }),
        )
    }
}
