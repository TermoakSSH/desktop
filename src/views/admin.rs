//! Server administration (administrators only): users (create, enable or
//! disable, grant or remove administrator, change the password, remove
//! two-step verification, list and revoke devices), invitations (create,
//! show the code and the link only once, revoke) and the paginated audit log.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::clipboard::Clipboard;
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_core::Id;
use termoak_core::model::{AuditEntry, Device, Invite, User};

use crate::qr;
use crate::runtime;
use crate::state::{AppModel, ModelEvent, api_error};
use crate::theme;
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Audit log entries per page.
const AUDIT_PAGE: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Users,
    Invites,
    Audit,
}

pub struct AdminView {
    model: Entity<AppModel>,
    tab: Tab,
    users: Vec<User>,
    invites: Vec<Invite>,
    audit: Vec<AuditEntry>,
    /// There are no older entries left.
    audit_done: bool,
    loading: bool,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

/// State of an invitation.
pub fn invite_status(inv: &Invite, now_ms: i64) -> (SharedString, InviteState) {
    if inv.revoked {
        (t!("admin.invite.state.revoked"), InviteState::Revoked)
    } else if inv.used_at.is_some() {
        (t!("admin.invite.state.used"), InviteState::Used)
    } else if inv.expires_at.is_some_and(|e| e <= now_ms) {
        (t!("admin.invite.state.expired"), InviteState::Expired)
    } else {
        (t!("admin.invite.state.pending"), InviteState::Pending)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InviteState {
    Pending,
    Used,
    Expired,
    Revoked,
}

/// Description of an audit log action.
pub fn action_label(action: &str) -> Option<SharedString> {
    Some(match action {
        "auth.login" => t!("admin.audit.action.auth_login"),
        "auth.register" => t!("admin.audit.action.auth_register"),
        "auth.password_changed" => t!("admin.audit.action.auth_password_changed"),
        "auth.2fa_enabled" => t!("admin.audit.action.auth_2fa_enabled"),
        "auth.2fa_disabled" => t!("admin.audit.action.auth_2fa_disabled"),
        "auth.device_revoked" => t!("admin.audit.action.auth_device_revoked"),
        "admin.user_created" => t!("admin.audit.action.admin_user_created"),
        "admin.user_updated" => t!("admin.audit.action.admin_user_updated"),
        "admin.password_reset" => t!("admin.audit.action.admin_password_reset"),
        "admin.2fa_reset" => t!("admin.audit.action.admin_2fa_reset"),
        "admin.device_revoked" => t!("admin.audit.action.admin_device_revoked"),
        "admin.invite_created" => t!("admin.audit.action.admin_invite_created"),
        "admin.invite_revoked" => t!("admin.audit.action.admin_invite_revoked"),
        "team.created" => t!("admin.audit.action.team_created"),
        "team.deleted" => t!("admin.audit.action.team_deleted"),
        "team.member_added" => t!("admin.audit.action.team_member_added"),
        "team.member_removed" => t!("admin.audit.action.team_member_removed"),
        "team.role_changed" => t!("admin.audit.action.team_role_changed"),
        "team.left" => t!("admin.audit.action.team_left"),
        "session.open" => t!("admin.audit.action.session_open"),
        "session.close" => t!("admin.audit.action.session_close"),
        "session.attach" => t!("admin.audit.action.session_attach"),
        "session.share" => t!("admin.audit.action.session_share"),
        "session.share_revoked" => t!("admin.audit.action.session_share_revoked"),
        "sftp.upload" => t!("admin.audit.action.sftp_upload"),
        "sftp.download" => t!("admin.audit.action.sftp_download"),
        "sftp.rename" => t!("admin.audit.action.sftp_rename"),
        "sftp.delete" => t!("admin.audit.action.sftp_delete"),
        "exec.batch" => t!("admin.audit.action.exec_batch"),
        "key.create" => t!("admin.audit.action.key_create"),
        "ai.task.mode" => t!("admin.audit.action.ai_task_mode"),
        "ai.approval.approved" => t!("admin.audit.action.ai_approval_approved"),
        "ai.approval.denied" => t!("admin.audit.action.ai_approval_denied"),
        _ => return None,
    })
}

/// Who did something, with the email if it is a known user.
pub fn actor_label(actor: &str, users: &[User]) -> String {
    match actor.split_once(':') {
        Some(("user", id)) => users
            .iter()
            .find(|u| u.id.to_string() == id)
            .map(|u| u.email.clone())
            .unwrap_or_else(|| t!("admin.actor.user", id = short_id(id)).to_string()),
        Some(("ai", task)) => t!("admin.actor.ai", id = short_id(task)).to_string(),
        Some(("guest", id)) => t!("admin.actor.guest", id = short_id(id)).to_string(),
        _ => actor.to_string(),
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

impl AdminView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.users.clear();
                    this.invites.clear();
                    this.audit.clear();
                    this.refresh(window, cx);
                }
            }),
        ];
        Self {
            model,
            tab: Tab::Users,
            users: Vec::new(),
            invites: Vec::new(),
            audit: Vec::new(),
            audit_done: false,
            loading: false,
            error: None,
            _subs: subs,
        }
    }

    fn api(&self, cx: &App) -> Option<termoak_client::ApiClient> {
        let m = self.model.read(cx);
        m.api.clone().filter(|_| m.is_admin())
    }

    /// Reloads the users and the visible tab.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        self.loading = true;
        self.error = None;
        cx.notify();
        let tab = self.tab;
        runtime::run_in(
            cx,
            window,
            async move {
                // Every tab needs the users (their emails).
                let users = api
                    .get::<Vec<User>>("/api/v1/admin/users")
                    .await
                    .map_err(api_error)?;
                let invites = match tab {
                    Tab::Invites => Some(
                        api.get::<Vec<Invite>>("/api/v1/admin/invites")
                            .await
                            .map_err(api_error)?,
                    ),
                    _ => None,
                };
                let audit = match tab {
                    Tab::Audit => Some(
                        api.get::<Vec<AuditEntry>>(&format!(
                            "/api/v1/admin/audit?limit={AUDIT_PAGE}"
                        ))
                        .await
                        .map_err(api_error)?,
                    ),
                    _ => None,
                };
                Ok::<_, String>((users, invites, audit))
            },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok((users, invites, audit)) => {
                        this.users = users;
                        if let Some(i) = invites {
                            this.invites = i;
                        }
                        if let Some(a) = audit {
                            this.audit_done = a.len() < AUDIT_PAGE;
                            this.audit = a;
                        }
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
        // Invitations can add the person to one of your teams.
        self.model.update(cx, |m, cx| m.refresh_teams(cx));
    }

    fn set_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        self.refresh(window, cx);
    }

    fn more_audit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(api), Some(last)) = (self.api(cx), self.audit.last().map(|e| e.id)) else {
            return;
        };
        self.loading = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<Vec<AuditEntry>>(&format!(
                    "/api/v1/admin/audit?limit={AUDIT_PAGE}&before={last}"
                ))
                .await
                .map_err(api_error)
            },
            |this, res, window, cx| {
                this.loading = false;
                match res {
                    Ok(page) => {
                        this.audit_done = page.len() < AUDIT_PAGE;
                        this.audit.extend(page);
                    }
                    Err(e) => ui::error(window, cx, t!("admin.audit.load_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    /// Administration request: reports the result and reloads.
    fn request(
        &mut self,
        fut: impl std::future::Future<Output = Result<Value, String>> + Send + 'static,
        ok: impl Into<SharedString>,
        fail: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ok = ok.into();
        let fail = fail.into();
        runtime::run_in(cx, window, fut, move |this, res, window, cx| {
            match res {
                Ok(_) => ui::success(window, cx, ok.clone()),
                Err(e) => ui::error(window, cx, format!("{fail}: {e}")),
            }
            this.refresh(window, cx);
        });
    }

    fn update_user(
        &mut self,
        user: &User,
        body: Value,
        ok: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let id = user.id;
        self.request(
            async move {
                api.patch::<Value>(&format!("/api/v1/admin/users/{id}"), &body)
                    .await
                    .map_err(api_error)
            },
            ok,
            t!("admin.user.update_failed"),
            window,
            cx,
        );
    }

    fn toggle_admin(&mut self, user: User, window: &mut Window, cx: &mut Context<Self>) {
        let make = !user.is_admin;
        let ok = if make {
            t!("admin.user.now_admin", email = user.email).to_string()
        } else {
            t!("admin.user.no_longer_admin", email = user.email).to_string()
        };
        self.update_user(&user, json!({"is_admin": make}), ok, window, cx);
    }

    fn toggle_disabled(&mut self, user: User, window: &mut Window, cx: &mut Context<Self>) {
        if user.disabled {
            let ok = t!("admin.user.enabled", email = user.email).to_string();
            self.update_user(&user, json!({"disabled": false}), ok, window, cx);
            return;
        }
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("admin.user.disable_title"),
            t!("admin.user.disable_confirm", email = user.email),
            t!("admin.user.disable"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    let user = user.clone();
                    v.update(cx, |v, cx| {
                        let ok = t!("admin.user.disabled", email = user.email).to_string();
                        v.update_user(&user, json!({"disabled": true}), ok, window, cx)
                    });
                }
            },
        );
    }

    fn reset_two_factor(&mut self, user: User, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("admin.user.reset_2fa_title"),
            t!("admin.user.reset_2fa_confirm", email = user.email),
            t!("common.remove"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                let user = user.clone();
                v.update(cx, |v, cx| {
                    let Some(api) = v.api(cx) else {
                        return;
                    };
                    let id = user.id;
                    v.request(
                        async move {
                            api.post::<Value>(
                                &format!("/api/v1/admin/users/{id}/2fa/reset"),
                                &json!({}),
                            )
                            .await
                            .map_err(api_error)
                        },
                        t!("admin.user.reset_2fa_done", email = user.email),
                        t!("admin.user.reset_2fa_failed"),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn reset_password(&mut self, user: User, window: &mut Window, cx: &mut Context<Self>) {
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("admin.password.new_placeholder"))
        });
        ui::focus_later(&password, window, cx);
        let p2 = password.clone();
        let weak = cx.entity().downgrade();
        let email = user.email.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("admin.password.title"),
            t!("admin.password.change"),
            440.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("admin.password.hint", email = email)),
                    )
                    .child(ui::field(
                        t!("admin.field.password"),
                        Input::new(&password).mask_toggle(),
                        cx,
                    ))
                    .into_any_element()
            },
            move |window, cx| {
                let password = p2.read(cx).value().to_string();
                if password.chars().count() < 8 {
                    ui::error(window, cx, t!("admin.password.too_short"));
                    return false;
                }
                let Some(v) = weak.upgrade() else {
                    return true;
                };
                let user = user.clone();
                v.update(cx, |v, cx| {
                    let Some(api) = v.api(cx) else {
                        return;
                    };
                    let id = user.id;
                    v.request(
                        async move {
                            api.post::<Value>(
                                &format!("/api/v1/admin/users/{id}/password"),
                                &json!({"password": password}),
                            )
                            .await
                            .map_err(api_error)
                        },
                        t!("admin.password.changed", email = user.email),
                        t!("admin.password.change_failed"),
                        window,
                        cx,
                    );
                });
                true
            },
        );
    }

    fn new_user(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = |window: &mut Window,
                     cx: &mut Context<Self>,
                     placeholder: SharedString,
                     masked: bool| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(masked)
                    .placeholder(placeholder)
            })
        };
        let email = input(window, cx, t!("admin.new_user.email_placeholder"), false);
        let name = input(window, cx, t!("admin.new_user.name_placeholder"), false);
        let password = input(window, cx, t!("admin.new_user.password_placeholder"), true);
        let admin = cx.new(|_| false);
        ui::focus_later(&email, window, cx);
        let (e2, n2, p2, a2) = (email.clone(), name.clone(), password.clone(), admin.clone());
        let weak = cx.entity().downgrade();
        ui::open_form_dialog(
            window,
            cx,
            t!("admin.new_user.title"),
            t!("common.create"),
            460.,
            move |_, cx| {
                let is_admin = *admin.read(cx);
                let toggle = admin.clone();
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("admin.field.email"), Input::new(&email), cx))
                    .child(ui::field(t!("common.name"), Input::new(&name), cx))
                    .child(ui::field(
                        t!("admin.field.password"),
                        Input::new(&password).mask_toggle(),
                        cx,
                    ))
                    .child(
                        Checkbox::new("new-user-admin")
                            .label(t!("admin.new_user.server_admin"))
                            .checked(is_admin)
                            .on_click(move |v, _, cx| {
                                toggle.update(cx, |a, cx| {
                                    *a = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("admin.new_user.invite_hint")),
                    )
                    .into_any_element()
            },
            move |window, cx| {
                let email = e2.read(cx).value().trim().to_string();
                let name = n2.read(cx).value().trim().to_string();
                let password = p2.read(cx).value().to_string();
                let is_admin = *a2.read(cx);
                if !email.contains('@') || password.chars().count() < 8 {
                    ui::error(window, cx, t!("admin.new_user.invalid"));
                    return false;
                }
                let Some(v) = weak.upgrade() else {
                    return true;
                };
                v.update(cx, |v, cx| {
                    let Some(api) = v.api(cx) else {
                        return;
                    };
                    let body = json!({"email": email, "name": name, "password": password, "is_admin": is_admin});
                    v.request(
                        async move {
                            api.post::<Value>("/api/v1/admin/users", &body)
                                .await
                                .map_err(api_error)
                        },
                        t!("admin.new_user.created", email = email),
                        t!("admin.new_user.create_failed"),
                        window,
                        cx,
                    );
                });
                true
            },
        );
    }

    fn open_devices(&mut self, user: User, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let dialog = cx.new(|cx| DevicesDialog::new(api, user.clone(), window, cx));
        window.open_dialog(cx, move |d, _, _| {
            d.title(t!("admin.devices.title", email = user.email))
                .w(px(600.))
                .child(dialog.clone())
                .footer(
                    h_flex().w_full().justify_end().child(
                        Button::new("devices-close")
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    fn new_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.api(cx).is_none() {
            return;
        }
        let weak = cx.entity().downgrade();
        let model = self.model.clone();
        let dialog = cx.new(|cx| InviteDialog::new(model, weak, window, cx));
        window.open_dialog(cx, move |d, _, _| {
            d.title(t!("admin.invite.new"))
                .w(px(620.))
                .overlay_closable(false)
                .child(dialog.clone())
        });
    }

    fn revoke_invite(&mut self, invite: Invite, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let who = invite
            .email
            .clone()
            .map(|e| t!("admin.invite.revoke_confirm_email", email = e))
            .unwrap_or_else(|| t!("admin.invite.revoke_confirm_any"));
        ui::confirm(
            window,
            cx,
            t!("admin.invite.revoke_title"),
            who,
            t!("admin.revoke"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                let id = invite.id;
                v.update(cx, |v, cx| {
                    let Some(api) = v.api(cx) else {
                        return;
                    };
                    v.request(
                        async move {
                            api.delete(&format!("/api/v1/admin/invites/{id}"))
                                .await
                                .map_err(api_error)
                        },
                        t!("admin.invite.revoked"),
                        t!("admin.invite.revoke_failed"),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn render_users(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let me = self.model.read(cx).me.as_ref().map(|u| u.id);
        let theme = cx.theme();
        let weak = cx.entity().downgrade();
        v_flex()
            .gap_2()
            .children(self.users.iter().enumerate().map(|(i, u)| {
                let is_me = Some(u.id) == me;
                let initials: String = u
                    .name
                    .split_whitespace()
                    .chain(std::iter::once(u.email.as_str()))
                    .filter_map(|w| w.chars().next())
                    .take(2)
                    .collect::<String>()
                    .to_uppercase();
                let user = u.clone();
                let w = weak.clone();
                h_flex()
                    .id(("user", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .when(u.disabled, |this| this.opacity(0.6))
                    .child(
                        div()
                            .size(px(36.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(theme::color_for(&u.email))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_xs()
                            .font_semibold()
                            .text_color(gpui::white())
                            .child(initials),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .child(div().text_sm().font_semibold().child(
                                        if u.name.is_empty() {
                                            u.email.clone()
                                        } else {
                                            u.name.clone()
                                        },
                                    ))
                                    .when(is_me, |this| {
                                        this.child(ui::pill(
                                            t!("admin.user.you"),
                                            theme.muted_foreground,
                                        ))
                                    })
                                    .when(u.is_admin, |this| {
                                        this.child(ui::pill(t!("admin.badge.admin"), theme.primary))
                                    })
                                    .when(u.totp_enabled, |this| {
                                        this.child(ui::pill("2FA", theme.success))
                                    })
                                    .when(u.disabled, |this| {
                                        this.child(ui::pill(
                                            t!("admin.badge.disabled"),
                                            theme.danger,
                                        ))
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                                "admin.user.joined",
                                email = u.email,
                                date = ui::format_ms(u.created_at)
                            ))),
                    )
                    .child(
                        Button::new(("user-menu", i))
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::EllipsisVertical))
                            .dropdown_menu(move |menu, _, _| {
                                let (w1, w2, w3, w4, w5) =
                                    (w.clone(), w.clone(), w.clone(), w.clone(), w.clone());
                                let (u1, u2, u3, u4, u5) = (
                                    user.clone(),
                                    user.clone(),
                                    user.clone(),
                                    user.clone(),
                                    user.clone(),
                                );
                                menu.item(
                                    PopupMenuItem::new(t!("admin.menu.devices"))
                                        .icon(ui::icon(IconName::Laptop))
                                        .on_click(move |_, window, cx| {
                                            if let Some(v) = w1.upgrade() {
                                                let u = u1.clone();
                                                v.update(cx, |v, cx| v.open_devices(u, window, cx));
                                            }
                                        }),
                                )
                                .item(
                                    PopupMenuItem::new(t!("admin.menu.change_password"))
                                        .icon(ui::icon(IconName::KeyRound))
                                        .on_click(move |_, window, cx| {
                                            if let Some(v) = w2.upgrade() {
                                                let u = u2.clone();
                                                v.update(cx, |v, cx| {
                                                    v.reset_password(u, window, cx)
                                                });
                                            }
                                        }),
                                )
                                .when(user.totp_enabled, |menu| {
                                    menu.item(
                                        PopupMenuItem::new(t!("admin.user.reset_2fa_title"))
                                            .icon(ui::icon(IconName::ShieldOff))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w3.upgrade() {
                                                    let u = u3.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.reset_two_factor(u, window, cx)
                                                    });
                                                }
                                            }),
                                    )
                                })
                                .separator()
                                .item(
                                    PopupMenuItem::new(if user.is_admin {
                                        t!("admin.menu.remove_admin")
                                    } else {
                                        t!("admin.menu.make_admin")
                                    })
                                    .icon(ui::icon(IconName::ShieldUser))
                                    .disabled(is_me)
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(v) = w4.upgrade() {
                                                let u = u4.clone();
                                                v.update(cx, |v, cx| v.toggle_admin(u, window, cx));
                                            }
                                        },
                                    ),
                                )
                                .item(
                                    PopupMenuItem::new(if user.disabled {
                                        t!("admin.menu.enable_account")
                                    } else {
                                        t!("admin.user.disable_title")
                                    })
                                    .icon(ui::icon(if user.disabled {
                                        IconName::CircleCheck
                                    } else {
                                        IconName::Ban
                                    }))
                                    .disabled(is_me)
                                    .on_click(
                                        move |_, window, cx| {
                                            if let Some(v) = w5.upgrade() {
                                                let u = u5.clone();
                                                v.update(cx, |v, cx| {
                                                    v.toggle_disabled(u, window, cx)
                                                });
                                            }
                                        },
                                    ),
                                )
                            }),
                    )
            }))
            .into_any_element()
    }

    fn render_invites(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let now = termoak_core::time::now_ms();
        let m = self.model.read(cx);
        let teams = m.teams.clone();
        let theme = cx.theme();
        if self.invites.is_empty() && !self.loading {
            return ui::empty_state(
                IconName::Ticket,
                t!("admin.invites.empty_title"),
                t!("admin.invites.empty_detail"),
                cx,
            )
            .into_any_element();
        }
        v_flex()
            .gap_2()
            .children(self.invites.iter().enumerate().map(|(i, inv)| {
                let (label, state) = invite_status(inv, now);
                let color = match state {
                    InviteState::Pending => theme.success,
                    InviteState::Used => theme.muted_foreground,
                    InviteState::Expired => theme.warning,
                    InviteState::Revoked => theme.danger,
                };
                let team = inv.team_id.map(|t| {
                    teams
                        .iter()
                        .find(|x| x.id == t)
                        .map(|x| t!("admin.team_named", name = x.name))
                        .unwrap_or_else(|| t!("admin.team_id", id = short_id(&t.to_string())))
                });
                let used_by = inv
                    .used_by
                    .and_then(|u| self.users.iter().find(|x| x.id == u))
                    .map(|u| u.email.clone());
                let mut details = vec![t!(
                    "admin.invite.created_on",
                    date = ui::format_ms(inv.created_at)
                )];
                match (state, inv.expires_at) {
                    (InviteState::Pending, Some(e)) => {
                        details.push(t!("admin.invite.expires_on", date = ui::format_ms(e)))
                    }
                    (InviteState::Pending, None) => details.push(t!("admin.invite.no_expiry")),
                    (InviteState::Expired, Some(e)) => {
                        details.push(t!("admin.invite.expired_on", date = ui::format_ms(e)))
                    }
                    _ => {}
                }
                if let (Some(at), Some(by)) = (inv.used_at, used_by) {
                    details.push(t!(
                        "admin.invite.used_by",
                        email = by,
                        date = ui::format_ms(at)
                    ));
                }
                let invite = inv.clone();
                h_flex()
                    .id(("invite", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(ui::icon(IconName::Ticket).size(px(18.)).text_color(color))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .child(div().text_sm().font_semibold().child(
                                        inv.email.clone().unwrap_or_else(|| {
                                            t!("admin.invite.any_email").to_string()
                                        }),
                                    ))
                                    .child(ui::pill(label, color))
                                    .when(inv.is_admin, |this| {
                                        this.child(ui::pill(t!("admin.badge.admin"), theme.primary))
                                    })
                                    .when_some(team, |this, t| this.child(ui::pill(t, theme.info))),
                            )
                            .child(
                                div().text_xs().text_color(theme.muted_foreground).child(
                                    details
                                        .iter()
                                        .map(|d| d.to_string())
                                        .collect::<Vec<_>>()
                                        .join(" · "),
                                ),
                            ),
                    )
                    .when(state == InviteState::Pending, |this| {
                        this.child(
                            Button::new(("revoke-invite", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Ban))
                                .label(t!("admin.revoke"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.revoke_invite(invite.clone(), window, cx)
                                })),
                        )
                    })
            }))
            .into_any_element()
    }

    fn render_audit(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let teams = self.model.read(cx).teams.clone();
        let target_label = |t: &str| -> String {
            if let Some(u) = self.users.iter().find(|u| u.id.to_string() == t) {
                return u.email.clone();
            }
            if let Some(team) = teams.iter().find(|x| x.id.to_string() == t) {
                return t!("admin.team_named", name = team.name).to_string();
            }
            if t.len() > 24 {
                format!("{}…", &t[..8])
            } else {
                t.to_string()
            }
        };
        v_flex()
            .gap_1()
            .when(self.audit.is_empty() && !self.loading, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("admin.audit.empty")),
                )
            })
            .children(self.audit.iter().enumerate().map(|(i, e)| {
                let detail =
                    if e.detail.as_object().is_some_and(|o| o.is_empty()) || e.detail.is_null() {
                        String::new()
                    } else {
                        let s = e.detail.to_string();
                        if s.chars().count() > 140 {
                            format!("{}…", s.chars().take(140).collect::<String>())
                        } else {
                            s
                        }
                    };
                h_flex()
                    .id(("audit", i))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .items_start()
                    .rounded(theme.radius)
                    .hover(|s| s.bg(theme.secondary_hover))
                    .child(
                        div()
                            .w(px(122.))
                            .flex_shrink_0()
                            .text_xs()
                            .font_family(mono.clone())
                            .text_color(theme.muted_foreground)
                            .child(ui::format_ms(e.created_at)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .child(
                                        div().text_sm().font_medium().child(
                                            action_label(&e.action)
                                                .unwrap_or_else(|| e.action.clone().into()),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(mono.clone())
                                            .text_color(theme.muted_foreground)
                                            .child(e.action.clone()),
                                    ),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{}{}",
                                    actor_label(&e.actor, &self.users),
                                    e.target
                                        .as_deref()
                                        .map(|t| format!(" → {}", target_label(t)))
                                        .unwrap_or_default()
                                ),
                            ))
                            .when(!detail.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .font_family(mono.clone())
                                        .text_color(theme.muted_foreground)
                                        .overflow_hidden()
                                        .child(detail),
                                )
                            }),
                    )
            }))
            .when(!self.audit.is_empty(), |this| {
                this.child(h_flex().pt_3().justify_center().child(if self.audit_done {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(tn!("admin.audit.end", self.audit.len()))
                        .into_any_element()
                } else {
                    Button::new("audit-more")
                        .icon(ui::icon(IconName::ChevronDown))
                        .label(t!("admin.audit.load_more"))
                        .loading(self.loading)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.more_audit(window, cx)
                        }))
                        .into_any_element()
                }))
            })
            .into_any_element()
    }
}

impl Render for AdminView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_admin = self.model.read(cx).is_admin();
        let tab = self.tab;
        let tab_button =
            |id: &'static str,
             label: SharedString,
             icon: IconName,
             t: Tab,
             cx: &mut Context<Self>| {
                Button::new(id)
                    .small()
                    .icon(ui::icon(icon))
                    .label(label)
                    .map(|b| if tab == t { b.primary() } else { b.ghost() })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_tab(t, window, cx)
                    }))
            };
        let tabs = h_flex()
            .px_6()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(tab_button(
                "tab-users",
                t!("admin.tab.users", count = self.users.len()),
                IconName::Users,
                Tab::Users,
                cx,
            ))
            .child(tab_button(
                "tab-invites",
                t!("admin.tab.invites"),
                IconName::Ticket,
                Tab::Invites,
                cx,
            ))
            .child(tab_button(
                "tab-audit",
                t!("admin.tab.audit"),
                IconName::ScrollText,
                Tab::Audit,
                cx,
            ));
        let action = match tab {
            Tab::Users => Button::new("admin-new-user")
                .primary()
                .icon(ui::icon(IconName::UserPlus))
                .label(t!("admin.new_user.title"))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.new_user(window, cx)))
                .into_any_element(),
            Tab::Invites => Button::new("admin-new-invite")
                .primary()
                .icon(ui::icon(IconName::Ticket))
                .label(t!("admin.invite.new"))
                .on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.new_invite(window, cx)),
                )
                .into_any_element(),
            Tab::Audit => div().into_any_element(),
        };
        let body: gpui::AnyElement = if !is_admin {
            ui::empty_state(
                IconName::ShieldUser,
                t!("admin.only_admins_title"),
                t!("admin.only_admins_detail"),
                cx,
            )
            .into_any_element()
        } else {
            let content = match tab {
                Tab::Users => self.render_users(cx),
                Tab::Invites => self.render_invites(cx),
                Tab::Audit => self.render_audit(cx),
            };
            v_flex()
                .gap_3()
                .when_some(self.error.clone(), |this, e| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(t!("admin.load_failed", error = e)),
                    )
                })
                .child(content)
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("admin.title"),
                t!("admin.subtitle"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("admin-refresh")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("common.refresh"))
                            .loading(self.loading)
                            .disabled(!is_admin)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.refresh(window, cx)
                            })),
                    )
                    .when(is_admin, |this| this.child(action)),
                cx,
            ))
            .when(is_admin, |this| this.child(tabs))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_6().max_w(px(980.)).child(body)),
                ),
            )
    }
}

/// Devices where a user is signed in.
struct DevicesDialog {
    api: termoak_client::ApiClient,
    user: User,
    devices: Vec<Device>,
    loading: bool,
    error: Option<String>,
}

impl DevicesDialog {
    fn new(
        api: termoak_client::ApiClient,
        user: User,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut d = Self {
            api,
            user,
            devices: Vec::new(),
            loading: false,
            error: None,
        };
        d.load(window, cx);
        d
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
        let api = self.api.clone();
        let id = self.user.id;
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<Vec<Device>>(&format!("/api/v1/admin/users/{id}/devices"))
                    .await
                    .map_err(api_error)
            },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(list) => {
                        this.devices = list;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn revoke(&mut self, device: Device, window: &mut Window, cx: &mut Context<Self>) {
        let api = self.api.clone();
        let (user, id) = (self.user.id, device.id);
        runtime::run_in(
            cx,
            window,
            async move {
                api.delete(&format!("/api/v1/admin/users/{user}/devices/{id}"))
                    .await
                    .map_err(api_error)
            },
            move |this, res, window, cx| {
                match res {
                    Ok(_) => {
                        ui::success(window, cx, t!("admin.devices.revoked", name = device.name))
                    }
                    Err(e) => ui::error(window, cx, t!("admin.devices.revoke_failed", error = e)),
                }
                this.load(window, cx);
            },
        );
    }
}

/// Icon for the platform of the device.
fn platform_icon(platform: &str) -> IconName {
    match platform {
        p if p.starts_with("desktop") => IconName::Laptop,
        "ios" | "android" => IconName::Smartphone,
        "cli" => IconName::SquareTerminal,
        _ => IconName::Monitor,
    }
}

impl Render for DevicesDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let now = termoak_core::time::now_ms();
        v_flex()
            .id("devices")
            .gap_2()
            .max_h(px(420.))
            .overflow_y_scroll()
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_sm().text_color(theme.danger).child(e))
            })
            .when(self.devices.is_empty() && !self.loading, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("admin.devices.empty")),
                )
            })
            .children(self.devices.iter().enumerate().map(|(i, d)| {
                let expired = d.refresh_expires_at <= now;
                let device = d.clone();
                h_flex()
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        ui::icon(platform_icon(&d.platform))
                            .size(px(18.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().text_sm().font_medium().child(d.name.clone()))
                                    .child(ui::pill(d.platform.clone(), theme.muted_foreground))
                                    .when(expired, |this| {
                                        this.child(ui::pill(
                                            t!("admin.devices.expired"),
                                            theme.warning,
                                        ))
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                                "admin.devices.last_used",
                                last = ui::format_ms(d.last_seen_at),
                                since = ui::format_ms(d.created_at)
                            ))),
                    )
                    .child(
                        Button::new(("revoke-device", i))
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::LogOut))
                            .label(t!("admin.revoke"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.revoke(device.clone(), window, cx)
                            })),
                    )
            }))
    }
}

/// Invitation just created (the code is only shown now).
#[derive(Clone)]
struct Created {
    token: String,
    link: String,
    server: String,
}

/// Invitation form and, afterwards, the code and the link.
struct InviteDialog {
    model: Entity<AppModel>,
    admin: WeakEntity<AdminView>,
    email: Entity<InputState>,
    team: ChoiceState<Option<Id>>,
    expiry: ChoiceState<i64>,
    is_admin: bool,
    busy: bool,
    created: Option<Created>,
}

impl InviteDialog {
    fn new(
        model: Entity<AppModel>,
        admin: WeakEntity<AdminView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let email = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("admin.invite.email_placeholder"))
        });
        let mut teams = vec![Choice::new(t!("common.none"), None)];
        teams.extend(
            model
                .read(cx)
                .teams
                .iter()
                .map(|t| Choice::new(t.name.clone(), Some(t.id))),
        );
        let team = ui::choice_state(teams, Some(&None), window, cx);
        let expiry = ui::choice_state(
            vec![
                Choice::new(t!("admin.invite.expiry_24h"), 24),
                Choice::new(t!("admin.invite.expiry_7d"), 24 * 7),
                Choice::new(t!("admin.invite.expiry_30d"), 24 * 30),
                Choice::new(t!("admin.invite.expiry_never"), 0),
            ],
            Some(&(24 * 7)),
            window,
            cx,
        );
        ui::focus_later(&email, window, cx);
        Self {
            model,
            admin,
            email,
            team,
            expiry,
            is_admin: false,
            busy: false,
            created: None,
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        let email = Some(self.email.read(cx).value().trim().to_string()).filter(|e| !e.is_empty());
        if email.as_deref().is_some_and(|e| !e.contains('@')) {
            ui::error(window, cx, t!("admin.invite.invalid_email"));
            return;
        }
        let team = ui::chosen(&self.team, cx).flatten();
        let hours = ui::chosen(&self.expiry, cx).unwrap_or(24 * 7);
        let body = json!({
            "email": email,
            "team_id": team,
            "is_admin": self.is_admin,
            "expires_in_hours": hours,
        });
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<Value>("/api/v1/admin/invites", &body)
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => {
                        this.created = Some(Created {
                            token: v["token"].as_str().unwrap_or("").to_string(),
                            link: v["url"].as_str().unwrap_or("").to_string(),
                            server: v["server"].as_str().unwrap_or("").to_string(),
                        });
                        if let Some(a) = this.admin.upgrade() {
                            a.update(cx, |a, cx| a.refresh(window, cx));
                        }
                    }
                    Err(e) => ui::error(window, cx, t!("admin.invite.create_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn render_created(&self, c: &Created, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let mut warn = theme.warning;
        warn.a = 0.12;
        let copy_row = |id: &'static str, label: SharedString, value: String| {
            ui::field(
                label,
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .px_3()
                            .py_2()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .font_family(mono.clone())
                            .text_xs()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(value.clone()),
                    )
                    .child(
                        Clipboard::new(id)
                            .small()
                            .value(value)
                            .tooltip(t!("common.copy")),
                    ),
                cx,
            )
        };
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .p_3()
                    .items_start()
                    .rounded(theme.radius)
                    .bg(warn)
                    .child(
                        ui::icon(IconName::TriangleAlert)
                            .size(px(16.))
                            .text_color(theme.warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .child(t!("admin.invite.copy_now")),
                    ),
            )
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(copy_row(
                                "copy-token",
                                t!("admin.invite.code"),
                                c.token.clone(),
                            ))
                            .child(copy_row(
                                "copy-link",
                                t!("admin.invite.app_link"),
                                c.link.clone(),
                            ))
                            .child(copy_row(
                                "copy-server",
                                t!("admin.invite.server"),
                                c.server.clone(),
                            )),
                    )
                    .when(!c.link.is_empty(), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    div()
                                        .p_1()
                                        .rounded(theme.radius)
                                        .bg(gpui::white())
                                        .child(qr::element(&c.link, 168.)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("admin.invite.qr_hint")),
                                ),
                        )
                    }),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("invite-done")
                        .primary()
                        .label(t!("admin.invite.done"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
            .into_any_element()
    }
}

impl Render for InviteDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(c) = self.created.clone() {
            return self.render_created(&c, cx);
        }
        let theme = cx.theme();
        let has_teams = self.model.read(cx).teams.len() > 0;
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("admin.invite.intro")),
            )
            .child(ui::field(
                t!("admin.field.email"),
                Input::new(&self.email),
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(div().flex_1().child(ui::field_with_hint(
                        t!("admin.invite.team"),
                        Select::new(&self.team).disabled(!has_teams),
                        t!("admin.invite.team_hint"),
                        cx,
                    )))
                    .child(div().w(px(170.)).child(ui::field(
                        t!("admin.invite.expires_in"),
                        Select::new(&self.expiry),
                        cx,
                    ))),
            )
            .child(
                Checkbox::new("invite-admin")
                    .label(t!("admin.invite.as_admin"))
                    .checked(self.is_admin)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.is_admin = *v;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("invite-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("invite-create")
                            .primary()
                            .icon(ui::icon(IconName::Ticket))
                            .label(t!("admin.invite.create"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.create(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::new_id;

    fn invite() -> Invite {
        Invite {
            id: new_id(),
            email: None,
            is_admin: false,
            team_id: None,
            team_role: None,
            created_by: new_id(),
            created_at: 0,
            expires_at: Some(1_000),
            used_by: None,
            used_at: None,
            revoked: false,
        }
    }

    #[test]
    fn invite_states() {
        let mut i = invite();
        assert_eq!(invite_status(&i, 500).1, InviteState::Pending);
        assert_eq!(invite_status(&i, 1_000).1, InviteState::Expired);
        i.expires_at = None;
        assert_eq!(invite_status(&i, i64::MAX).1, InviteState::Pending);
        i.used_at = Some(10);
        assert_eq!(invite_status(&i, 0).1, InviteState::Used);
        i.revoked = true;
        let (label, state) = invite_status(&i, 0);
        assert_eq!(
            (label.to_string(), state),
            ("Revoked".to_string(), InviteState::Revoked)
        );
    }

    #[test]
    fn audit_labels() {
        assert_eq!(
            action_label("auth.login").map(|s| s.to_string()),
            Some("Sign-in".to_string())
        );
        assert_eq!(action_label("something.else"), None);
        let ana = User {
            id: new_id(),
            email: "ana@example.com".into(),
            name: "Ana".into(),
            is_admin: true,
            created_at: 0,
            disabled: false,
            totp_enabled: false,
            plan: "free".into(),
            email_verified: true,
            locale: "en".into(),
        };
        assert_eq!(
            actor_label(&format!("user:{}", ana.id), std::slice::from_ref(&ana)),
            "ana@example.com"
        );
        assert_eq!(actor_label("user:0123456789abcdef", &[]), "user 01234567");
        assert_eq!(actor_label("ai:abcdefghij", &[]), "AI (task abcdefgh)");
        assert_eq!(actor_label("system", &[]), "system");
    }
}
