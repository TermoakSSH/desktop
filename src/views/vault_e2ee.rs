//! A vault's Encryption tab (0.7 phase 5b, docs-next/E2EE-SSO-AUDIT.md §2.4,
//! §2.6, §2.7 and §10): turn end-to-end encryption on (who gets the key
//! now, with their fingerprints; what stops working; the private network
//! warning) or off (typed confirmation), rotate the key, the "someone lost
//! access" banner, and members waiting for the key confirmed by
//! fingerprint (with the warning when their key changed since this
//! computer last gave them one).

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::Account;
use termoak_client::e2ee::PendingMember;
use termoak_core::Id;
use termoak_core::model::{E2eeVaultState, Vault, VaultCrypto};

use crate::e2ee::E2eeBook;
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, IconName};

fn muted(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// The lines of "what changes" when a vault becomes end-to-end encrypted.
pub fn consequences() -> Vec<SharedString> {
    vec![
        t!("vault_e2ee.consequence.server_sessions"),
        t!("vault_e2ee.consequence.reveal"),
        t!("vault_e2ee.consequence.jit"),
        t!("vault_e2ee.consequence.ai"),
        t!("vault_e2ee.consequence.moves"),
        t!("vault_e2ee.consequence.certificates"),
        t!("vault_e2ee.consequence.metadata"),
    ]
}

pub struct VaultE2eeView {
    model: Entity<AppModel>,
    account: Id,
    vault_id: Id,
    manager: bool,
    state: Option<E2eeVaultState>,
    state_error: Option<String>,
    pending: Vec<PendingMember>,
    /// Members who get the key when turning it on (unchecked ones wait).
    include: BTreeSet<Id>,
    confirm_off: Entity<InputState>,
    busy: bool,
    /// What the vault said when it was last loaded (crypto, key version,
    /// pending members, rekey): a change loads again.
    seen: Option<(bool, u32, usize, bool, bool)>,
    _subs: Vec<Subscription>,
}

impl VaultE2eeView {
    pub fn new(
        model: Entity<AppModel>,
        account: Id,
        vault: Id,
        manager: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let book = E2eeBook::global(cx);
        let name = model
            .read(cx)
            .vault_entry(account, vault)
            .map(crate::accounts::VaultEntry::label)
            .unwrap_or_default();
        let confirm_off = cx.new(|cx| InputState::new(window, cx).placeholder(name));
        let subs = vec![
            cx.observe(&book, |_, _, cx| cx.notify()),
            cx.observe_in(&model, window, |this, _, window, cx| {
                if this.fingerprint_of_vault(cx) != this.seen {
                    this.load(window, cx);
                }
                cx.notify();
            }),
        ];
        let mut this = Self {
            model,
            account,
            vault_id: vault,
            manager,
            state: None,
            state_error: None,
            pending: Vec::new(),
            include: BTreeSet::new(),
            confirm_off,
            busy: false,
            seen: None,
            _subs: subs,
        };
        this.load(window, cx);
        this
    }

    fn acc(&self, cx: &App) -> Option<Arc<Account>> {
        self.model
            .read(cx)
            .ws
            .account(self.account)
            .filter(|a| a.is_signed_in())
    }

    fn vault(&self, cx: &App) -> Option<Vault> {
        self.model
            .read(cx)
            .vault_entry(self.account, self.vault_id)
            .map(|e| e.vault.clone())
    }

    fn fingerprint_of_vault(&self, cx: &App) -> Option<(bool, u32, usize, bool, bool)> {
        self.vault(cx).map(|v| {
            let e = v.e2ee.unwrap_or_default();
            (
                v.crypto == VaultCrypto::E2ee,
                e.key_version,
                e.pending.len(),
                e.rekey_needed,
                e.wrap.is_some(),
            )
        })
    }

    fn unlocked(&self, cx: &App) -> bool {
        self.model
            .read(cx)
            .e2ee_of(self.account, cx)
            .is_some_and(|s| s.unlocked)
    }

    fn set_up(&self, cx: &App) -> bool {
        self.model
            .read(cx)
            .e2ee_of(self.account, cx)
            .is_some_and(|s| s.set_up())
    }

    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let vault = self.vault_id;
        let holder = self
            .vault(cx)
            .and_then(|v| v.e2ee)
            .is_some_and(|e| e.wrap.is_some());
        let may_state = self.manager || holder;
        self.seen = self.fingerprint_of_vault(cx);
        runtime::run_in(
            cx,
            window,
            async move {
                let state = if may_state {
                    Some(acc.e2ee_vault_state(vault).await.map_err(api_error)?)
                } else {
                    None
                };
                // Asked of the server (the local copy of the vault may not
                // say yet that it is end-to-end encrypted, right after
                // turning it on): only key holders get pending members.
                let pending = if may_state {
                    acc.e2ee_pending_members(vault).await.unwrap_or_default()
                } else {
                    Vec::new()
                };
                Ok::<_, String>((state, pending))
            },
            |this, res, _, cx| {
                match res {
                    Ok((state, pending)) => {
                        if let Some(s) = &state
                            && this.include.is_empty()
                        {
                            this.include = s.recipients.iter().map(|r| r.user_id).collect();
                        }
                        this.state = state;
                        this.pending = pending;
                        this.state_error = None;
                    }
                    Err(e) => this.state_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn after(&mut self, ok: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        ui::success(window, cx, ok);
        let account = self.account;
        self.model.update(cx, |m, cx| {
            m.sync_account(account, cx);
            m.refresh_e2ee(account, cx);
        });
        self.load(window, cx);
    }

    fn enable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let (vault, include) = (
            self.vault_id,
            self.include.iter().copied().collect::<Vec<_>>(),
        );
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                acc.e2ee_enable_vault(vault, Some(include))
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(_) => this.after(t!("vault_e2ee.enabled"), window, cx),
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .vault(cx)
            .map(|v| crate::accounts::vault_label(&v))
            .unwrap_or_default();
        if self.confirm_off.read(cx).value().trim() != name.trim() {
            return ui::error(window, cx, t!("vault_e2ee.off_mismatch"));
        }
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let vault = self.vault_id;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_disable_vault(vault).await.map_err(api_error) },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(()) => {
                        this.confirm_off
                            .update(cx, |i, cx| i.set_value("", window, cx));
                        this.after(t!("vault_e2ee.disabled"), window, cx)
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn rotate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let vault = self.vault_id;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_rotate_vault(vault).await.map_err(api_error) },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => this.after(t!("vault_e2ee.rotated", version = v), window, cx),
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn confirm_members(
        &mut self,
        users: Option<Vec<Id>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let vault = self.vault_id;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.e2ee_add_members(vault, users).await.map_err(api_error) },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(n) => this.after(tn!("vault_e2ee.confirmed", n), window, cx),
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    /// Networks of the vault (their keys stay readable by the server).
    fn networks(&self, cx: &App) -> usize {
        let vault = self.vault_id;
        let m = self.model.read(cx);
        let personal = m.account(self.account).and_then(|a| a.info.user_id);
        m.networks
            .iter()
            .filter(|n| {
                n.account() == Some(self.account) && n.meta.vault_id.or(personal) == Some(vault)
            })
            .count()
    }

    fn render_network_warning(&self, cx: &App) -> Option<gpui::AnyElement> {
        let n = self.networks(cx);
        (n > 0).then(|| {
            let theme = cx.theme();
            h_flex()
                .gap_2()
                .items_start()
                .p_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.warning)
                .child(ui::icon(IconName::TriangleAlert).text_color(theme.warning))
                .child(div().text_sm().child(tn!("vault_e2ee.networks_warning", n)))
                .into_any_element()
        })
    }

    fn render_needs_key(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let set_up = self.set_up(cx);
        if set_up && self.unlocked(cx) {
            return None;
        }
        let model = self.model.clone();
        let account = self.account;
        Some(
            h_flex()
                .gap_3()
                .items_center()
                .child(muted(
                    if set_up {
                        t!("vault_e2ee.unlock_first")
                    } else {
                        t!("vault_e2ee.setup_first")
                    },
                    cx,
                ))
                .child(
                    Button::new("vault-e2ee-key")
                        .small()
                        .primary()
                        .label(if set_up {
                            t!("e2ee.unlock.button")
                        } else {
                            t!("e2ee.setup.button")
                        })
                        .on_click(move |_: &ClickEvent, window, cx| {
                            if set_up {
                                super::e2ee::open_unlock(model.clone(), account, window, cx)
                            } else {
                                super::e2ee::open_setup(model.clone(), account, window, cx)
                            }
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_off(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let needs_key = self.render_needs_key(cx);
        let theme = cx.theme();
        // What changes goes last: the decision (who gets the key, the
        // button) stays in sight.
        let what_changes = v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("vault_e2ee.consequences_title")),
            )
            .children(consequences().into_iter().map(|c| {
                div()
                    .text_sm()
                    .pl_3()
                    .text_color(theme.muted_foreground)
                    .child(format!("• {c}"))
            }));
        let mut col = v_flex()
            .gap_3()
            .child(muted(t!("vault_e2ee.off_intro"), cx))
            .children(self.render_network_warning(cx));
        if !self.manager {
            return col
                .child(muted(t!("vault_e2ee.managers_only"), cx))
                .child(what_changes)
                .into_any_element();
        }
        if let Some(e) = needs_key {
            return col.child(e).child(what_changes).into_any_element();
        }
        let Some(state) = &self.state else {
            return col
                .child(match &self.state_error {
                    Some(e) => div().text_sm().text_color(theme.danger).child(e.clone()),
                    None => h_flex()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(muted(t!("common.loading"), cx)),
                })
                .child(what_changes)
                .into_any_element();
        };
        let me = self
            .model
            .read(cx)
            .account(self.account)
            .and_then(|a| a.info.user_id);
        col = col
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("vault_e2ee.recipients_title")),
            )
            .children(state.recipients.iter().enumerate().map(|(i, r)| {
                let user = r.user_id;
                let mine = Some(user) == me;
                let on = mine || self.include.contains(&user);
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Checkbox::new(("e2ee-include", i))
                            .checked(on)
                            .disabled(mine)
                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                if *v {
                                    this.include.insert(user);
                                } else {
                                    this.include.remove(&user);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(if mine {
                                t!("vault_e2ee.you", email = r.email.clone()).to_string()
                            } else if r.name.is_empty() {
                                r.email.clone()
                            } else {
                                format!("{} · {}", r.name, r.email)
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(ui::mono_family(cx))
                                    .text_color(theme.muted_foreground)
                                    .child(r.fingerprint.clone()),
                            ),
                    )
            }))
            .child(muted(t!("vault_e2ee.recipients_hint"), cx))
            .when(!state.without_key.is_empty(), |this| {
                this.child(muted(
                    tn!("vault_e2ee.without_key", state.without_key.len()),
                    cx,
                ))
            })
            .child(muted(tn!("vault_e2ee.items", state.items.len()), cx))
            .child(
                h_flex().justify_end().child(
                    Button::new("vault-e2ee-on")
                        .primary()
                        .icon(ui::icon(IconName::ShieldLock))
                        .label(t!("vault_e2ee.turn_on"))
                        .loading(self.busy)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.enable(window, cx)),
                        ),
                ),
            )
            .child(what_changes);
        col.into_any_element()
    }

    fn render_on(&self, vault: &Vault, cx: &mut Context<Self>) -> gpui::AnyElement {
        let info = vault.e2ee.clone().unwrap_or_default();
        let needs_key = self
            .render_needs_key(cx)
            .filter(|_| info.wrap.is_some() || info.may_hold_key);
        let theme = cx.theme();
        let holder = info.wrap.is_some();
        let unlocked = self.unlocked(cx);
        let mut col = v_flex().gap_3().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(ui::pill(t!("vault_e2ee.badge"), theme.success))
                .child(muted(
                    t!("vault_e2ee.key_version", version = info.key_version),
                    cx,
                )),
        );
        if info.rekey_needed && holder {
            col = col.child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.warning)
                    .child(ui::icon(IconName::TriangleAlert).text_color(theme.warning))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(t!("vault_e2ee.rekey_needed")),
                    )
                    .child(
                        Button::new("vault-e2ee-rekey")
                            .small()
                            .primary()
                            .label(t!("vault_e2ee.rotate"))
                            .loading(self.busy)
                            .disabled(!unlocked)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.rotate(window, cx)
                            })),
                    ),
            );
        }
        if !info.may_hold_key {
            col = col.child(muted(t!("vault_e2ee.no_key_role"), cx));
        } else if !holder {
            let fp = self
                .model
                .read(cx)
                .e2ee_of(self.account, cx)
                .and_then(|s| s.key.map(|k| k.fingerprint));
            col = col.child(muted(
                match fp {
                    Some(fp) => t!("vault_e2ee.waiting_for_key", fingerprint = fp),
                    None => t!("vault_e2ee.setup_to_get_key"),
                },
                cx,
            ));
        }
        if let Some(e) = needs_key {
            col = col.child(e);
        }
        if holder && !self.pending.is_empty() {
            col = col
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(t!("vault_e2ee.pending_title")),
                )
                .child(muted(t!("vault_e2ee.pending_hint"), cx))
                .children(self.pending.iter().enumerate().map(|(i, p)| {
                    let r = &p.recipient;
                    let user = r.user_id;
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(if p.key_changed {
                            theme.danger
                        } else {
                            theme.border
                        })
                        .child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(div().text_sm().font_medium().child(
                                            if r.name.is_empty() {
                                                r.email.clone()
                                            } else {
                                                format!("{} · {}", r.name, r.email)
                                            },
                                        ))
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_family(ui::mono_family(cx))
                                                .child(r.fingerprint.clone()),
                                        ),
                                )
                                .child(
                                    Button::new(("e2ee-confirm", i))
                                        .small()
                                        .primary()
                                        .label(t!("vault_e2ee.confirm"))
                                        .disabled(!unlocked || self.busy)
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.confirm_members(Some(vec![user]), window, cx)
                                            },
                                        )),
                                ),
                        )
                        .when(p.key_changed, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.danger)
                                    .child(t!("vault_e2ee.key_changed")),
                            )
                        })
                }));
        }
        let while_on = v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("vault_e2ee.on_consequences_title")),
            )
            .children(consequences().into_iter().map(|c| {
                div()
                    .text_sm()
                    .pl_3()
                    .text_color(theme.muted_foreground)
                    .child(format!("• {c}"))
            }));
        col = col.children(self.render_network_warning(cx));
        if self.manager {
            let name = crate::accounts::vault_label(vault);
            col = col
                .child(
                    h_flex().gap_2().child(
                        Button::new("vault-e2ee-rotate")
                            .icon(ui::icon(IconName::RotateCcwKey))
                            .label(t!("vault_e2ee.rotate"))
                            .disabled(!holder || !unlocked || self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.rotate(window, cx)
                            })),
                    ),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .pt_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(t!("vault_e2ee.off_title")),
                        )
                        .child(muted(t!("vault_e2ee.off_message"), cx))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().w(px(260.)).child(ui::field(
                                    t!("vault_e2ee.off_type", name = name),
                                    Input::new(&self.confirm_off),
                                    cx,
                                )))
                                .child(
                                    Button::new("vault-e2ee-off")
                                        .danger()
                                        .label(t!("vault_e2ee.turn_off"))
                                        .loading(self.busy)
                                        .disabled(!holder || !unlocked)
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.disable(window, cx)
                                            },
                                        )),
                                ),
                        ),
                );
        }
        col.child(while_on).into_any_element()
    }
}

impl Render for VaultE2eeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(vault) = self.vault(cx) else {
            return div().into_any_element();
        };
        if vault.crypto == VaultCrypto::E2ee {
            self.render_on(&vault, cx)
        } else {
            self.render_off(cx)
        }
    }
}
