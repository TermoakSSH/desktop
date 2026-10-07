//! Vaults: creating one (yours or a team's), and the management dialog of a
//! vault: name, description, color and icon; members (people by email and
//! teams) with their role, Editor or Use only; the Strict switch (Use-only
//! members connect only through the server); its activity; leaving it, and
//! deleting it with a typed confirmation that says what goes with it.
//!
//! Every change is made on the account's server; a sync then brings the new
//! vault list to this device.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::switch::Switch;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::json;
use termoak_client::ApiClient;
use termoak_core::Id;
use termoak_core::model::{
    AuditEntry, Team, Vault, VaultKind, VaultMember, VaultPrincipal, VaultRole,
};

use crate::accounts::{VaultEntry, vault_label};
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::theme;
use crate::ui::{self, IconName};

/// Colors offered for a vault.
pub const VAULT_COLORS: [&str; 8] = [
    "#4f7cff", "#30a46c", "#f5a524", "#e5484d", "#8e4ec6", "#0ea5e9", "#d6409f", "#12a594",
];

/// Icons offered for a vault (Lucide names, as stored).
pub const VAULT_ICONS: [&str; 10] = [
    "vault",
    "server",
    "building-2",
    "briefcase",
    "database",
    "cloud",
    "users",
    "shield-check",
    "key-round",
    "box",
];

fn icon_by_name(name: &str) -> Option<IconName> {
    Some(match name {
        "vault" => IconName::Vault,
        "server" => IconName::Server,
        "building-2" => IconName::Building2,
        "briefcase" => IconName::Briefcase,
        "database" => IconName::Database,
        "cloud" => IconName::Cloud,
        "users" => IconName::Users,
        "shield-check" => IconName::ShieldCheck,
        "key-round" => IconName::KeyRound,
        "box" => IconName::Box,
        _ => return None,
    })
}

/// Icon of a vault: its own, otherwise by kind.
pub fn vault_icon(v: &Vault) -> IconName {
    v.icon
        .as_deref()
        .and_then(icon_by_name)
        .unwrap_or(match v.kind {
            VaultKind::Personal => IconName::User,
            VaultKind::Team => IconName::Users,
            _ => IconName::Vault,
        })
}

/// Color of a vault: its own, otherwise one derived from its name.
pub fn vault_color(v: &Vault) -> Hsla {
    v.color
        .as_deref()
        .and_then(theme::parse_color)
        .unwrap_or_else(|| theme::color_for(&v.name))
}

/// Name of a role.
pub fn role_label(role: VaultRole) -> SharedString {
    match role {
        VaultRole::Manager => t!("vaults.role.manager"),
        VaultRole::Editor => t!("vaults.role.editor"),
        VaultRole::UseOnly => t!("vaults.role.use_only"),
        VaultRole::Unknown => t!("vaults.role.unknown"),
    }
}

fn role_color(role: VaultRole, cx: &App) -> Hsla {
    let theme = cx.theme();
    match role {
        VaultRole::Manager => theme.warning,
        VaultRole::Editor => theme.primary,
        _ => theme.muted_foreground,
    }
}

/// Small chip of a vault (host cards, lists).
pub fn chip(v: &VaultEntry, cx: &App) -> gpui::Div {
    let color = vault_color(&v.vault);
    let mut bg = color;
    bg.a = 0.14;
    h_flex()
        .gap_1()
        .items_center()
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .bg(bg)
        .text_color(color)
        .text_xs()
        .font_medium()
        .max_w(px(140.))
        .child(ui::icon(vault_icon(&v.vault)).size(px(11.)).flex_shrink_0())
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(v.label()),
        )
        .when(!v.can_write(), |this| {
            this.child(
                ui::icon(IconName::Lock)
                    .size(px(10.))
                    .text_color(cx.theme().muted_foreground),
            )
        })
}

/// Name of the item counts of a vault ("3 hosts, 1 key").
pub fn counts_text(v: &Vault) -> String {
    let order = [
        ("host", "vaults.count.host"),
        ("group", "vaults.count.group"),
        ("key", "vaults.count.key"),
        ("identity", "vaults.count.identity"),
        ("snippet", "vaults.count.snippet"),
        ("forward", "vaults.count.forward"),
    ];
    let parts: Vec<String> = order
        .iter()
        .filter_map(|(kind, key)| {
            let n = v.item_counts.get(*kind).copied().unwrap_or(0);
            (n > 0).then(|| {
                let key = crate::i18n::plural_key(key, n as u64);
                rust_i18n::t!(key.as_str(), count = n).to_string()
            })
        })
        .collect();
    if parts.is_empty() {
        t!("vaults.count.empty").to_string()
    } else {
        parts.join(", ")
    }
}

/// Text of an audit action of a vault.
fn audit_action(action: &str) -> String {
    let key = format!("vaults.audit.{}", action.replace(['.', '-'], "_"));
    crate::_rust_i18n_try_translate(&crate::i18n::current(), &key)
        .map(|s| s.into_owned())
        .unwrap_or_else(|| action.to_string())
}

/// Dialog to create a vault: yours (shared later) or of a team.
pub fn open_create(
    model: Entity<AppModel>,
    account: Id,
    team: Option<Team>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(api) = model.read(cx).api_of(Some(account)) else {
        ui::error(window, cx, t!("vaults.not_signed_in"));
        return;
    };
    let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("vaults.name_placeholder")));
    let description =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("vaults.description_placeholder")));
    let member_role = ui::choice_state(
        vec![
            ui::Choice::new(role_label(VaultRole::Editor), Some(VaultRole::Editor)),
            ui::Choice::new(role_label(VaultRole::UseOnly), Some(VaultRole::UseOnly)),
            ui::Choice::new(t!("vaults.team_role.none"), None),
        ],
        Some(&Some(VaultRole::Editor)),
        window,
        cx,
    );
    ui::focus_later(&name, window, cx);
    let (n2, d2, r2) = (name.clone(), description.clone(), member_role.clone());
    let team_name = team.as_ref().map(|t| t.name.clone());
    ui::open_form_dialog(
        window,
        cx,
        match &team_name {
            Some(t) => t!("vaults.new_team_title", team = t.clone()),
            None => t!("vaults.new_title"),
        },
        t!("vaults.create"),
        460.,
        move |_, cx| {
            v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(if team_name.is_some() {
                            t!("vaults.new_team_hint")
                        } else {
                            t!("vaults.new_hint")
                        }),
                )
                .child(ui::field(t!("common.name"), Input::new(&name), cx))
                .child(ui::field(
                    t!("vaults.description"),
                    Input::new(&description),
                    cx,
                ))
                .when(team_name.is_some(), |this| {
                    this.child(ui::field_with_hint(
                        t!("vaults.team_role.label"),
                        gpui_component::select::Select::new(&member_role),
                        t!("vaults.team_role.hint"),
                        cx,
                    ))
                })
                .into_any_element()
        },
        move |window, cx| {
            let name = n2.read(cx).value().trim().to_string();
            if name.is_empty() {
                ui::error(window, cx, t!("vaults.error.name"));
                return false;
            }
            let description = d2.read(cx).value().trim().to_string();
            let mut body = json!({"name": name, "description": description});
            if let Some(team) = &team {
                body["team_id"] = json!(team.id);
                body["team_member_role"] = json!(ui::chosen(&r2, cx).flatten());
            }
            let api = api.clone();
            let model = model.clone();
            let task = runtime::spawn(cx, async move {
                api.create_vault(&body).await.map_err(api_error)
            });
            window
                .spawn(cx, async move |cx| {
                    let res = task.await;
                    let _ = cx.update(|window, cx| match res {
                        Ok(v) => {
                            ui::success(window, cx, t!("vaults.created", name = v.name.clone()));
                            model.update(cx, |m, cx| m.sync_account(account, cx));
                        }
                        Err(e) => ui::error(window, cx, t!("vaults.error.create", error = e)),
                    });
                })
                .detach();
            true
        },
    );
}

/// Opens the management dialog of a vault.
pub fn open_manage(
    model: Entity<AppModel>,
    account: Id,
    vault: Id,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(entry) = model.read(cx).vault_entry(account, vault).cloned() else {
        return;
    };
    let title = entry.label();
    let view = cx.new(|cx| VaultManager::new(model, entry, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("vaults.manage_title", name = title.clone()))
            .w(px(620.))
            .child(view.clone())
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VaultTab {
    General,
    Members,
    Activity,
}

/// Adding a member: a person by email or a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddWho {
    Person,
    Team,
}

struct VaultManager {
    model: Entity<AppModel>,
    account: Id,
    vault: Vault,
    role: VaultRole,
    tab: VaultTab,
    name: Entity<InputState>,
    description: Entity<InputState>,
    color: Option<String>,
    icon: Option<String>,
    strict: bool,
    team_member_role: Option<VaultRole>,
    members: Vec<VaultMember>,
    members_error: Option<String>,
    loading: bool,
    add_who: AddWho,
    add_email: Entity<InputState>,
    add_team: Option<Id>,
    add_role: VaultRole,
    teams: Vec<Team>,
    audit: Vec<AuditEntry>,
    audit_error: Option<String>,
    audit_more: bool,
    delete_confirm: Entity<InputState>,
    busy: bool,
    _subs: Vec<gpui::Subscription>,
}

impl VaultManager {
    fn new(
        model: Entity<AppModel>,
        entry: VaultEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let v = entry.vault.clone();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("vaults.name_placeholder"))
                .default_value(v.name.clone())
        });
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("vaults.description_placeholder"))
                .default_value(v.description.clone())
        });
        let add_email =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("teams.email_placeholder")));
        let delete_confirm = cx.new(|cx| InputState::new(window, cx).placeholder(v.name.clone()));
        let subs = vec![
            cx.subscribe_in(
                &add_email,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.add_member(window, cx);
                    }
                },
            ),
            cx.subscribe(&delete_confirm, |_, _, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            model,
            account: entry.account,
            role: entry.role(),
            strict: entry.strict(),
            team_member_role: v.team_member_role,
            color: v.color.clone(),
            icon: v.icon.clone(),
            vault: v,
            tab: VaultTab::General,
            name,
            description,
            members: Vec::new(),
            members_error: None,
            loading: false,
            add_who: AddWho::Person,
            add_email,
            add_team: None,
            add_role: VaultRole::Editor,
            teams: Vec::new(),
            audit: Vec::new(),
            audit_error: None,
            audit_more: false,
            delete_confirm,
            busy: false,
            _subs: subs,
        };
        this.load_members(window, cx);
        this.load_teams(window, cx);
        this
    }

    fn api(&self, cx: &App) -> Option<ApiClient> {
        self.model.read(cx).api_of(Some(self.account))
    }

    fn personal(&self) -> bool {
        self.vault.kind == VaultKind::Personal
    }

    fn manager(&self) -> bool {
        self.role == VaultRole::Manager
    }

    /// After a change: the account syncs (new vault list) and the dialog
    /// shows the server's answer.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let account = self.account;
        self.model.update(cx, |m, cx| m.sync_account(account, cx));
    }

    fn load_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            self.members_error = Some(t!("vaults.not_signed_in").to_string());
            return;
        };
        if self.personal() {
            return;
        }
        let id = self.vault.id;
        self.loading = true;
        runtime::run_in(
            cx,
            window,
            async move { api.vault_members(id).await.map_err(api_error) },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(list) => {
                        this.members = list;
                        this.members_error = None;
                    }
                    Err(e) => this.members_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn load_teams(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<Vec<Team>>("/api/v1/teams")
                    .await
                    .map_err(api_error)
            },
            |this, res, _, cx| {
                if let Ok(teams) = res {
                    // You can share with teams you belong to.
                    this.teams = teams.into_iter().filter(|t| t.role.is_some()).collect();
                    cx.notify();
                }
            },
        );
    }

    fn load_audit(&mut self, more: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let id = self.vault.id;
        let before = more.then(|| self.audit.last().map(|e| e.id)).flatten();
        const PAGE: u32 = 50;
        runtime::run_in(
            cx,
            window,
            async move { api.vault_audit(id, before, PAGE).await.map_err(api_error) },
            move |this, res, _, cx| {
                match res {
                    Ok(list) => {
                        this.audit_more = list.len() as u32 == PAGE;
                        if more {
                            this.audit.extend(list);
                        } else {
                            this.audit = list;
                        }
                        this.audit_error = None;
                    }
                    Err(e) => this.audit_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn set_tab(&mut self, tab: VaultTab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        if tab == VaultTab::Activity && self.audit.is_empty() {
            self.load_audit(false, window, cx);
        }
        cx.notify();
    }

    /// Runs a request against the vault and, on success, shows `ok`.
    fn request<T: Send + 'static>(
        &mut self,
        fut: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
        ok: SharedString,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        cx.notify();
        runtime::run_in(cx, window, fut, move |this, res, window, cx| {
            this.busy = false;
            match res {
                Ok(v) => {
                    if !ok.is_empty() {
                        ui::success(window, cx, ok);
                    }
                    done(this, v, window, cx);
                    this.changed(cx);
                }
                Err(e) => ui::error(window, cx, e),
            }
            cx.notify();
        });
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            ui::error(window, cx, t!("vaults.error.name"));
            return;
        }
        let mut patch = json!({
            "name": name,
            "color": self.color,
            "icon": self.icon,
        });
        if !self.personal() {
            patch["description"] = json!(self.description.read(cx).value().trim());
            patch["settings"] = json!({"use_only_local": !self.strict});
            if self.vault.kind == VaultKind::Team {
                patch["team_member_role"] = json!(self.team_member_role);
            }
        }
        let id = self.vault.id;
        self.request(
            async move { api.update_vault(id, &patch).await.map_err(api_error) },
            t!("vaults.saved"),
            |this, v: Vault, _, cx| {
                this.vault = v;
                cx.notify();
            },
            window,
            cx,
        );
    }

    fn add_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let role = self.add_role;
        let body = match self.add_who {
            AddWho::Person => {
                let email = self.add_email.read(cx).value().trim().to_string();
                if email.is_empty() {
                    ui::error(window, cx, t!("vaults.error.email"));
                    return;
                }
                json!({"email": email, "role": role})
            }
            AddWho::Team => {
                let Some(team) = self.add_team else {
                    ui::error(window, cx, t!("vaults.error.team"));
                    return;
                };
                json!({"team_id": team, "role": role})
            }
        };
        let id = self.vault.id;
        self.request(
            async move { api.add_vault_member(id, &body).await.map_err(api_error) },
            t!("vaults.member_added"),
            |this, _: VaultMember, window, cx| {
                this.add_email
                    .update(cx, |i, cx| i.set_value("", window, cx));
                this.load_members(window, cx);
            },
            window,
            cx,
        );
    }

    fn set_member_role(
        &mut self,
        member: Id,
        role: VaultRole,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let id = self.vault.id;
        self.request(
            async move {
                api.set_vault_member_role(id, member, role)
                    .await
                    .map_err(api_error)
            },
            t!("vaults.role_changed"),
            |this, _: VaultMember, window, cx| this.load_members(window, cx),
            window,
            cx,
        );
    }

    fn remove_member(&mut self, member: VaultMember, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let who = principal_name(&member.principal);
        ui::confirm(
            window,
            cx,
            t!("vaults.remove_member_title"),
            t!("vaults.remove_member_message", name = who),
            t!("vaults.remove_member"),
            true,
            move |window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| {
                    let Some(api) = this.api(cx) else {
                        return;
                    };
                    let (id, m) = (this.vault.id, member.id);
                    this.request(
                        async move { api.remove_vault_member(id, m).await.map_err(api_error) },
                        t!("vaults.member_removed"),
                        |this, _: (), window, cx| this.load_members(window, cx),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("vaults.leave_title"),
            t!("vaults.leave_message", name = self.vault.name.clone()),
            t!("vaults.leave"),
            true,
            move |window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                view.update(cx, |this, cx| {
                    let Some(api) = this.api(cx) else {
                        return;
                    };
                    let id = this.vault.id;
                    this.request(
                        async move { api.leave_vault(id).await.map_err(api_error) },
                        t!("vaults.left"),
                        |_, _: (), window, cx| window.close_dialog(cx),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let typed = self.delete_confirm.read(cx).value().trim().to_string();
        if typed != self.vault.name {
            ui::error(window, cx, t!("vaults.delete_mismatch"));
            return;
        }
        let id = self.vault.id;
        let name = self.vault.name.clone();
        self.request(
            async move { api.delete_vault(id, &typed).await.map_err(api_error) },
            t!("vaults.deleted", name = name),
            |_, _: (), window, cx| window.close_dialog(cx),
            window,
            cx,
        );
    }

    // ----- Rendering -----

    fn render_general(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let manager = self.manager();
        let personal = self.personal();
        let v = &self.vault;
        let muted = theme.muted_foreground;
        let ring = theme.foreground;
        let border = theme.border;
        let info = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(ui::pill(role_label(self.role), role_color(self.role, cx)))
                    .child(ui::pill(
                        match v.kind {
                            VaultKind::Personal => t!("vaults.kind.personal"),
                            VaultKind::Team => t!("vaults.kind.team"),
                            _ => t!("vaults.kind.shared"),
                        },
                        muted,
                    ))
                    .when(self.strict && !personal, |this| {
                        this.child(ui::pill(t!("vaults.strict_badge"), theme.warning))
                    }),
            )
            .child(div().text_sm().text_color(muted).child(counts_text(v)))
            .when_some(v.owner_name.clone(), |this, owner| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("vaults.owner", name = owner)),
                )
            });
        if !manager {
            return v_flex()
                .gap_4()
                .child(info)
                .when(!v.description.is_empty(), |this| {
                    this.child(div().text_sm().child(v.description.clone()))
                })
                .child(div().text_sm().text_color(muted).child(match self.role {
                    VaultRole::UseOnly if self.strict => t!("vaults.you_use_only_strict"),
                    VaultRole::UseOnly => t!("vaults.you_use_only"),
                    _ => t!("vaults.you_editor"),
                }))
                .when(!personal, |this| {
                    this.child(
                        h_flex().child(
                            Button::new("leave-vault")
                                .icon(ui::icon(IconName::DoorOpen))
                                .label(t!("vaults.leave"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.leave(window, cx)
                                })),
                        ),
                    )
                })
                .into_any_element();
        }
        let selected_color = self.color.clone();
        let colors = h_flex()
            .gap_2()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .id("vault-color-none")
                    .size(px(22.))
                    .rounded_full()
                    .border_2()
                    .border_color(if selected_color.is_none() {
                        ring
                    } else {
                        border
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(ui::icon(IconName::Ban).size(px(12.)).text_color(muted))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.color = None;
                        cx.notify();
                    })),
            )
            .children(VAULT_COLORS.iter().enumerate().map(|(i, hex)| {
                let color = theme::parse_color(hex).unwrap_or(muted);
                let active = selected_color
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(hex));
                div()
                    .id(("vault-color", i))
                    .size(px(22.))
                    .rounded_full()
                    .border_2()
                    .border_color(if active { ring } else { color })
                    .bg(color)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.color = Some(hex.to_string());
                        cx.notify();
                    }))
            }));
        let selected_icon = self.icon.clone();
        let icons = h_flex()
            .gap_1()
            .flex_wrap()
            .children(VAULT_ICONS.iter().enumerate().map(|(i, name)| {
                let active = selected_icon.as_deref() == Some(*name);
                Button::new(("vault-icon", i))
                    .small()
                    .icon(ui::icon(icon_by_name(name).unwrap_or(IconName::Vault)))
                    .map(|b| if active { b.primary() } else { b.ghost() })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.icon = if this.icon.as_deref() == Some(*name) {
                            None
                        } else {
                            Some(name.to_string())
                        };
                        cx.notify();
                    }))
            }));
        let typed = self.delete_confirm.read(cx).value().trim().to_string();
        let can_delete = typed == v.name;
        v_flex()
            .gap_4()
            .child(info)
            .child(ui::field(t!("common.name"), Input::new(&self.name), cx))
            .when(!personal, |this| {
                this.child(ui::field(
                    t!("vaults.description"),
                    Input::new(&self.description),
                    cx,
                ))
            })
            .child(ui::field(t!("vaults.color"), colors, cx))
            .child(ui::field(t!("vaults.icon"), icons, cx))
            .when(v.kind == VaultKind::Team, |this| {
                let current = self.team_member_role;
                this.child(ui::field_with_hint(
                    t!("vaults.team_role.label"),
                    h_flex().gap_1().children(
                        [
                            (Some(VaultRole::Editor), role_label(VaultRole::Editor)),
                            (Some(VaultRole::UseOnly), role_label(VaultRole::UseOnly)),
                            (None, t!("vaults.team_role.none")),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(i, (role, label))| {
                            Button::new(("team-member-role", i))
                                .small()
                                .label(label)
                                .map(|b| {
                                    if current == role {
                                        b.primary()
                                    } else {
                                        b.ghost()
                                    }
                                })
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.team_member_role = role;
                                    cx.notify();
                                }))
                        }),
                    ),
                    t!("vaults.team_role.hint"),
                    cx,
                ))
            })
            .when(!personal, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Switch::new("vault-strict")
                                .label(t!("vaults.strict"))
                                .checked(self.strict)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.strict = *v;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("vaults.strict_hint")),
                        ),
                )
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("save-vault")
                        .primary()
                        .icon(ui::icon(IconName::Save))
                        .label(t!("common.save"))
                        .loading(self.busy)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.save(window, cx)),
                        ),
                ),
            )
            .when(!personal, |this| {
                this.child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.danger)
                        .child(
                            div()
                                .font_semibold()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(t!("vaults.delete_title")),
                        )
                        .child(div().text_sm().child(t!(
                            "vaults.delete_message",
                            counts = counts_text(v),
                            members = v.member_count
                        )))
                        .child(ui::field(
                            t!("vaults.delete_type", name = v.name.clone()),
                            Input::new(&self.delete_confirm),
                            cx,
                        ))
                        .child(
                            h_flex().child(
                                Button::new("delete-vault")
                                    .danger()
                                    .icon(ui::icon(IconName::Trash))
                                    .label(t!("vaults.delete"))
                                    .disabled(!can_delete)
                                    .loading(self.busy)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.delete(window, cx)
                                    })),
                            ),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_members(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        if self.personal() {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("vaults.personal_not_shared"))
                .into_any_element();
        }
        let manager = self.manager();
        let weak = cx.entity().downgrade();
        // You among the members: your email masked when emails are hidden;
        // the others' as they are.
        let me = self
            .model
            .read(cx)
            .account(self.account)
            .and_then(|a| a.info.user_id);
        let names = crate::accounts::names();
        let rows = self.members.iter().enumerate().map(|(i, m)| {
            let (icon, title, detail) = match &m.principal {
                VaultPrincipal::User { id, email, name } if Some(*id) == me => (
                    IconName::User,
                    if name.trim().is_empty() {
                        names.email(email)
                    } else {
                        name.clone()
                    },
                    (!name.trim().is_empty()).then(|| names.email(email)),
                ),
                VaultPrincipal::User { email, name, .. } => (
                    IconName::User,
                    if name.trim().is_empty() {
                        email.clone()
                    } else {
                        name.clone()
                    },
                    (!name.trim().is_empty()).then(|| email.clone()),
                ),
                VaultPrincipal::Team { name, .. } => (
                    IconName::Users,
                    name.clone(),
                    Some(t!("vaults.team_member").to_string()),
                ),
                VaultPrincipal::Unknown => (IconName::User, "?".to_string(), None),
            };
            let editable = manager && !m.implicit;
            let member = m.clone();
            let (w1, w2) = (weak.clone(), weak.clone());
            h_flex()
                .gap_3()
                .p_2()
                .items_center()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .child(
                    ui::icon(icon)
                        .size(px(16.))
                        .text_color(theme.muted_foreground),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_medium().child(title))
                        .when_some(detail, |this, d| {
                            this.child(div().text_xs().text_color(theme.muted_foreground).child(d))
                        }),
                )
                .when(m.implicit, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("vaults.implicit")),
                    )
                })
                .map(|this| {
                    if editable {
                        let member_id = member.id;
                        this.child(
                            Button::new(("member-role", i))
                                .small()
                                .label(role_label(member.role))
                                .dropdown_menu(move |menu, _, _| {
                                    let mut menu = menu;
                                    for role in [VaultRole::Editor, VaultRole::UseOnly] {
                                        let w = w1.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(role_label(role))
                                                .checked(member.role == role)
                                                .on_click(move |_, window, cx| {
                                                    if let Some(v) = w.upgrade() {
                                                        v.update(cx, |v, cx| {
                                                            v.set_member_role(
                                                                member_id, role, window, cx,
                                                            )
                                                        });
                                                    }
                                                }),
                                        );
                                    }
                                    let w = w2.clone();
                                    let member = member.clone();
                                    menu.separator().item(
                                        PopupMenuItem::new(t!("vaults.remove_member"))
                                            .icon(ui::icon(IconName::UserMinus))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w.upgrade() {
                                                    let member = member.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.remove_member(member, window, cx)
                                                    });
                                                }
                                            }),
                                    )
                                }),
                        )
                    } else {
                        this.child(ui::pill(role_label(m.role), role_color(m.role, cx)))
                    }
                })
        });
        let add = manager.then(|| {
            let teams = self.teams.clone();
            let team_label = self
                .add_team
                .and_then(|t| teams.iter().find(|x| x.id == t))
                .map(|t| t.name.clone())
                .unwrap_or_else(|| t!("vaults.choose_team").to_string());
            let view = cx.entity().downgrade();
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(t!("vaults.add_member")),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("add-person")
                                .small()
                                .icon(ui::icon(IconName::User))
                                .label(t!("vaults.add_person"))
                                .map(|b| {
                                    if self.add_who == AddWho::Person {
                                        b.primary()
                                    } else {
                                        b.ghost()
                                    }
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.add_who = AddWho::Person;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("add-team")
                                .small()
                                .icon(ui::icon(IconName::Users))
                                .label(t!("vaults.add_team"))
                                .disabled(teams.is_empty())
                                .map(|b| {
                                    if self.add_who == AddWho::Team {
                                        b.primary()
                                    } else {
                                        b.ghost()
                                    }
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.add_who = AddWho::Team;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w_0().map(|this| {
                            match self.add_who {
                                AddWho::Person => this.child(Input::new(&self.add_email)),
                                AddWho::Team => this.child(
                                    Button::new("choose-team")
                                        .w_full()
                                        .label(team_label)
                                        .dropdown_menu(move |mut menu, _, _| {
                                            for t in &teams {
                                                let w = view.clone();
                                                let id = t.id;
                                                menu = menu.item(
                                                    PopupMenuItem::new(t.name.clone()).on_click(
                                                        move |_, _, cx| {
                                                            if let Some(v) = w.upgrade() {
                                                                v.update(cx, |v, cx| {
                                                                    v.add_team = Some(id);
                                                                    cx.notify();
                                                                });
                                                            }
                                                        },
                                                    ),
                                                );
                                            }
                                            menu
                                        }),
                                ),
                            }
                        }))
                        .children(
                            [VaultRole::Editor, VaultRole::UseOnly]
                                .into_iter()
                                .enumerate()
                                .map(|(i, role)| {
                                    Button::new(("add-role", i))
                                        .small()
                                        .label(role_label(role))
                                        .map(|b| {
                                            if self.add_role == role {
                                                b.primary()
                                            } else {
                                                b.ghost()
                                            }
                                        })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _, cx| {
                                                this.add_role = role;
                                                cx.notify();
                                            },
                                        ))
                                }),
                        )
                        .child(
                            Button::new("add-member")
                                .primary()
                                .icon(ui::icon(IconName::UserPlus))
                                .label(t!("vaults.add"))
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.add_member(window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("vaults.roles_hint")),
                )
        });
        v_flex()
            .gap_2()
            .children(add)
            .when_some(self.members_error.clone(), |this, e| {
                this.child(div().text_sm().text_color(theme.danger).child(e))
            })
            .when(self.loading && self.members.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("common.loading")),
                )
            })
            .children(rows)
            .into_any_element()
    }

    fn render_activity(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        if !self.manager() {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("vaults.activity_managers"))
                .into_any_element();
        }
        let who = |actor: &str| -> String {
            let id = actor
                .strip_prefix("user:")
                .and_then(|s| s.parse::<Id>().ok());
            id.and_then(|id| {
                self.members.iter().find_map(|m| match &m.principal {
                    VaultPrincipal::User { id: u, email, .. } if *u == id => Some(email.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| actor.to_string())
        };
        v_flex()
            .gap_1()
            .when_some(self.audit_error.clone(), |this, e| {
                this.child(div().text_sm().text_color(theme.danger).child(e))
            })
            .when(
                self.audit.is_empty() && self.audit_error.is_none(),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("vaults.activity_empty")),
                    )
                },
            )
            .children(self.audit.iter().map(|e| {
                h_flex()
                    .gap_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_sm()
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(ui::format_ms(e.created_at)),
                    )
                    .child(div().flex_1().min_w_0().child(audit_action(&e.action)))
                    .child(
                        div()
                            .max_w(px(180.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(who(&e.actor)),
                    )
            }))
            .when(self.audit_more, |this| {
                this.child(
                    Button::new("audit-more")
                        .small()
                        .ghost()
                        .label(t!("common.load_more"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.load_audit(true, window, cx)
                        })),
                )
            })
            .into_any_element()
    }
}

/// Name of a member (person or team).
fn principal_name(p: &VaultPrincipal) -> String {
    match p {
        VaultPrincipal::User { email, .. } => email.clone(),
        VaultPrincipal::Team { name, .. } => name.clone(),
        VaultPrincipal::Unknown => "?".into(),
    }
}

impl Render for VaultManager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = TabBar::new("vault-tabs")
            .selected_index(match self.tab {
                VaultTab::General => 0,
                VaultTab::Members => 1,
                VaultTab::Activity => 2,
            })
            .child(Tab::new().label(t!("vaults.tab.general")))
            .child(Tab::new().label(tn!("vaults.tab.members", self.members.len())))
            .child(Tab::new().label(t!("vaults.tab.activity")))
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                let tab = match *ix {
                    1 => VaultTab::Members,
                    2 => VaultTab::Activity,
                    _ => VaultTab::General,
                };
                this.set_tab(tab, window, cx);
            }));
        let body = match self.tab {
            VaultTab::General => self.render_general(cx),
            VaultTab::Members => self.render_members(cx),
            VaultTab::Activity => self.render_activity(cx),
        };
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(
                ui::icon(vault_icon(&self.vault))
                    .size(px(18.))
                    .text_color(vault_color(&self.vault)),
            )
            .child(div().font_semibold().child(vault_label(&self.vault)));
        v_flex().gap_3().child(header).child(tabs).child(
            v_flex()
                .id("vault-body")
                .max_h(px(520.))
                .overflow_y_scrollbar()
                .child(body),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(kind: &str, icon: Option<&str>, counts: serde_json::Value) -> Vault {
        serde_json::from_value(json!({
            "id": Id::nil(), "kind": kind, "name": "Ops", "icon": icon,
            "created_by": Id::nil(), "created_at": 0, "updated_at": 0,
            "item_counts": counts
        }))
        .unwrap()
    }

    #[test]
    fn icons_and_counts() {
        assert_eq!(vault_icon(&vault("team", None, json!({}))), IconName::Users);
        assert_eq!(
            vault_icon(&vault("personal", None, json!({}))),
            IconName::User
        );
        assert_eq!(
            vault_icon(&vault("shared", Some("database"), json!({}))),
            IconName::Database
        );
        assert_eq!(
            vault_icon(&vault("shared", Some("nope"), json!({}))),
            IconName::Vault
        );
        assert!(VAULT_ICONS.iter().all(|n| icon_by_name(n).is_some()));
        assert_eq!(
            counts_text(&vault(
                "team",
                None,
                json!({"host": 3, "key": 1, "memory": 4})
            )),
            "3 hosts, 1 key"
        );
        assert_eq!(counts_text(&vault("team", None, json!({}))), "no items");
        assert_eq!(audit_action("vault.member_add"), "Member added");
        assert_eq!(audit_action("something.new"), "something.new");
    }
}
