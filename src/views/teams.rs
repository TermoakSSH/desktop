//! Server teams: yours, with your role and their members. Create, rename and
//! delete teams; add members by email, change their role, remove them and
//! leave a team. Only what your role allows is offered:
//! - member: view and leave;
//! - admin: also rename and manage members (without touching the owners);
//! - owner (or server administrator): everything, including deleting the
//!   team and appointing owners.
//!
//! The "Vaults" tab lists the team's vaults (team owners and admins create
//! them); deleting a team says which vaults go with it.

use gpui::{
    AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::select::Select;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_core::Id;
use termoak_core::model::{Team, TeamMember, TeamRole};

use super::vaults;
use crate::accounts::VaultEntry;
use crate::runtime;
use crate::state::{AppModel, ModelEvent, api_error};
use crate::theme;
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Name of the role for the interface.
pub fn role_label(role: TeamRole) -> SharedString {
    match role {
        TeamRole::Member => t!("teams.role.member"),
        TeamRole::Admin => t!("teams.role.admin"),
        TeamRole::Owner => t!("teams.role.owner"),
    }
}

/// Menu action that gives a member the role `role`.
fn make_role_label(role: TeamRole) -> SharedString {
    match role {
        TeamRole::Member => t!("teams.make.member"),
        TeamRole::Admin => t!("teams.make.admin"),
        TeamRole::Owner => t!("teams.make.owner"),
    }
}

/// Notice after giving `email` the role `role`.
fn role_changed_text(email: &str, role: TeamRole) -> SharedString {
    match role {
        TeamRole::Member => t!("teams.role_changed.member", email = email),
        TeamRole::Admin => t!("teams.role_changed.admin", email = email),
        TeamRole::Owner => t!("teams.role_changed.owner", email = email),
    }
}

/// What someone with the role `mine` in a team can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub rename: bool,
    pub delete: bool,
    pub add_members: bool,
    /// Can add or appoint owners.
    pub grant_owner: bool,
}

impl Permissions {
    pub fn of(mine: Option<TeamRole>) -> Self {
        let manage = mine.is_some_and(|r| r >= TeamRole::Admin);
        let owner = mine == Some(TeamRole::Owner);
        Self {
            rename: manage,
            delete: owner,
            add_members: manage,
            grant_owner: owner,
        }
    }

    /// Can they change the role of someone with `target` to `to`?
    pub fn can_set_role(&self, mine: Option<TeamRole>, target: TeamRole, to: TeamRole) -> bool {
        let manage = mine.is_some_and(|r| r >= TeamRole::Admin);
        let owner = mine == Some(TeamRole::Owner);
        manage && target != to && (owner || (target != TeamRole::Owner && to != TeamRole::Owner))
    }

    /// Can they remove someone with the role `target` from the team?
    pub fn can_remove(&self, mine: Option<TeamRole>, target: TeamRole) -> bool {
        let manage = mine.is_some_and(|r| r >= TeamRole::Admin);
        manage && (target != TeamRole::Owner || mine == Some(TeamRole::Owner))
    }
}

/// Tabs of a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamTab {
    Members,
    Vaults,
}

pub struct TeamsView {
    model: Entity<AppModel>,
    selected: Option<Id>,
    tab: TeamTab,
    members: Vec<TeamMember>,
    loading_members: bool,
    members_error: Option<String>,
    add_email: Entity<InputState>,
    add_role: ChoiceState<TeamRole>,
    /// Roles of the "Add" dropdown (they change if you are an owner).
    add_roles_owner: bool,
    busy: bool,
    _subs: Vec<Subscription>,
}

fn role_choices(owner: bool) -> Vec<Choice<TeamRole>> {
    let mut v = vec![
        Choice::new(role_label(TeamRole::Member), TeamRole::Member),
        Choice::new(role_label(TeamRole::Admin), TeamRole::Admin),
    ];
    if owner {
        v.push(Choice::new(role_label(TeamRole::Owner), TeamRole::Owner));
    }
    v
}

impl TeamsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let add_email =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("teams.email_placeholder")));
        let add_role = ui::choice_state(role_choices(false), Some(&TeamRole::Member), window, cx);
        let subs = vec![
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.sync_selection(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.selected = None;
                    this.members.clear();
                    this.refresh(window, cx);
                }
            }),
            cx.subscribe_in(
                &add_email,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.add_member(window, cx);
                    }
                },
            ),
        ];
        Self {
            model,
            selected: None,
            tab: TeamTab::Members,
            members: Vec::new(),
            loading_members: false,
            members_error: None,
            add_email,
            add_role,
            add_roles_owner: false,
            busy: false,
            _subs: subs,
        }
    }

    /// Reloads the teams (and the members of the selected one).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh_teams(cx));
        if self.selected.is_some() {
            self.load_members(window, cx);
        }
    }

    fn team(&self, cx: &gpui::App) -> Option<Team> {
        let id = self.selected?;
        self.model
            .read(cx)
            .teams
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    fn my_role(&self, team: &Team, cx: &gpui::App) -> Option<TeamRole> {
        self.model.read(cx).team_role(team)
    }

    /// Selects a team if none is selected (or the selected one is gone).
    fn sync_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let teams: Vec<Id> = self.model.read(cx).teams.iter().map(|t| t.id).collect();
        let valid = self.selected.is_some_and(|s| teams.contains(&s));
        if !valid {
            self.selected = None;
            self.members.clear();
            if let Some(first) = teams.first().copied() {
                self.select(first, window, cx);
            }
        }
        // The roles dropdown depends on whether you are an owner.
        let owner = self
            .team(cx)
            .is_some_and(|t| self.my_role(&t, cx) == Some(TeamRole::Owner));
        if owner != self.add_roles_owner {
            self.add_roles_owner = owner;
            self.add_role.update(cx, |s, cx| {
                s.set_items(role_choices(owner), window, cx);
                s.set_selected_value(&TeamRole::Member, window, cx);
            });
        }
    }

    fn select(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            self.members.clear();
            self.members_error = None;
            self.add_email
                .update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.load_members(window, cx);
        self.sync_selection(window, cx);
        cx.notify();
    }

    fn load_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(api), Some(id)) = (self.model.read(cx).api.clone(), self.selected) else {
            return;
        };
        self.loading_members = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<Vec<TeamMember>>(&format!("/api/v1/teams/{id}/members"))
                    .await
                    .map_err(api_error)
            },
            move |this, res, _, cx| {
                if this.selected != Some(id) {
                    return;
                }
                this.loading_members = false;
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

    /// Sends a request and, when it finishes, notifies and reloads.
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
        self.busy = true;
        cx.notify();
        runtime::run_in(cx, window, fut, move |this, res, window, cx| {
            this.busy = false;
            match res {
                Ok(v) => {
                    // Member operations return the new list.
                    if let Ok(list) = serde_json::from_value::<Vec<TeamMember>>(v) {
                        this.members = list;
                    }
                    if !ok.is_empty() {
                        ui::success(window, cx, ok.clone());
                    }
                    this.refresh(window, cx);
                }
                Err(e) => ui::error(
                    window,
                    cx,
                    t!("teams.request_failed", action = fail, error = e),
                ),
            }
            cx.notify();
        });
    }

    fn new_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.team_name_dialog(None, window, cx);
    }

    /// Dialog to create (or rename) a team.
    fn team_name_dialog(
        &mut self,
        team: Option<Team>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("teams.name_placeholder"))
                .default_value(team.as_ref().map(|t| t.name.clone()).unwrap_or_default())
        });
        ui::focus_later(&name, window, cx);
        let n2 = name.clone();
        let weak = cx.entity().downgrade();
        let renaming = team.as_ref().map(|t| t.id);
        ui::open_form_dialog(
            window,
            cx,
            if renaming.is_some() {
                t!("teams.rename_title")
            } else {
                t!("teams.new_team")
            },
            if renaming.is_some() {
                t!("common.save")
            } else {
                t!("common.create")
            },
            420.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .when(renaming.is_none(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("teams.new_team_hint")),
                        )
                    })
                    .child(ui::field(t!("common.name"), Input::new(&name), cx))
                    .into_any_element()
            },
            move |window, cx| {
                let name = n2.read(cx).value().trim().to_string();
                if name.is_empty() {
                    ui::error(window, cx, t!("teams.name_required"));
                    return false;
                }
                let Some(view) = weak.upgrade() else {
                    return true;
                };
                view.update(cx, |this, cx| {
                    let Some(api) = this.model.read(cx).api.clone() else {
                        return;
                    };
                    match renaming {
                        Some(id) => {
                            let body = json!({"name": name});
                            this.request(
                                async move {
                                    api.patch::<Value>(&format!("/api/v1/teams/{id}"), &body)
                                        .await
                                        .map_err(api_error)
                                },
                                t!("teams.renamed"),
                                t!("teams.rename_failed"),
                                window,
                                cx,
                            );
                        }
                        None => {
                            let body = json!({"name": name.clone()});
                            let task = runtime::spawn(cx, async move {
                                api.post::<Team>("/api/v1/teams", &body)
                                    .await
                                    .map_err(api_error)
                            });
                            cx.spawn_in(window, async move |this, cx| {
                                let res = task.await;
                                let _ = this.update_in(cx, |this, window, cx| match res {
                                    Ok(team) => {
                                        ui::success(
                                            window,
                                            cx,
                                            t!("teams.created", name = team.name),
                                        );
                                        this.selected = Some(team.id);
                                        this.members.clear();
                                        this.refresh(window, cx);
                                        this.load_members(window, cx);
                                    }
                                    Err(e) => {
                                        ui::error(window, cx, t!("teams.create_failed", error = e))
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                });
                true
            },
        );
    }

    fn delete_team(&mut self, team: Team, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        // Deleting a team deletes its vaults and their items.
        let vaults = self.team_vaults(team.id, cx);
        let items: i64 = vaults.iter().map(VaultEntry::item_count).sum();
        let message = if vaults.is_empty() {
            t!("teams.delete_confirm", name = team.name.clone())
        } else {
            t!(
                "teams.delete_confirm_vaults",
                name = team.name.clone(),
                vaults = vaults
                    .iter()
                    .map(VaultEntry::label)
                    .collect::<Vec<_>>()
                    .join(", "),
                items = items
            )
        };
        ui::confirm(
            window,
            cx,
            t!("teams.delete_title"),
            message,
            t!("teams.delete"),
            true,
            move |window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let id = team.id;
                let name = team.name.clone();
                view.update(cx, |this, cx| {
                    let Some(api) = this.model.read(cx).api.clone() else {
                        return;
                    };
                    this.selected = None;
                    this.members.clear();
                    this.request(
                        async move {
                            api.delete(&format!("/api/v1/teams/{id}"))
                                .await
                                .map_err(api_error)
                        },
                        t!("teams.deleted", name = name),
                        t!("teams.delete_failed"),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn leave_team(&mut self, team: Team, window: &mut Window, cx: &mut Context<Self>) {
        let Some(me) = self.model.read(cx).me.as_ref().map(|u| u.id) else {
            return;
        };
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("teams.leave"),
            t!("teams.leave_confirm", name = team.name),
            t!("teams.leave_ok"),
            true,
            move |window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let id = team.id;
                let name = team.name.clone();
                view.update(cx, |this, cx| {
                    let Some(api) = this.model.read(cx).api.clone() else {
                        return;
                    };
                    this.request(
                        async move {
                            api.delete(&format!("/api/v1/teams/{id}/members/{me}"))
                                .await
                                .map_err(api_error)
                        },
                        t!("teams.left", name = name),
                        t!("teams.leave_failed"),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn add_member(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(api), Some(id)) = (self.model.read(cx).api.clone(), self.selected) else {
            return;
        };
        let email = self.add_email.read(cx).value().trim().to_string();
        if !email.contains('@') {
            ui::error(window, cx, t!("teams.email_required"));
            return;
        }
        let role = ui::chosen(&self.add_role, cx).unwrap_or(TeamRole::Member);
        self.add_email
            .update(cx, |i, cx| i.set_value("", window, cx));
        let body = json!({"email": email, "role": role.as_str()});
        self.request(
            async move {
                api.post::<Value>(&format!("/api/v1/teams/{id}/members"), &body)
                    .await
                    .map_err(api_error)
            },
            t!("teams.member_added", email = email),
            t!("teams.add_failed"),
            window,
            cx,
        );
    }

    fn set_role(
        &mut self,
        member: TeamMember,
        role: TeamRole,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(api), Some(id)) = (self.model.read(cx).api.clone(), self.selected) else {
            return;
        };
        let user = member.user_id;
        let body = json!({"role": role.as_str()});
        self.request(
            async move {
                api.patch::<Value>(&format!("/api/v1/teams/{id}/members/{user}"), &body)
                    .await
                    .map_err(api_error)
            },
            role_changed_text(&member.email, role),
            t!("teams.role_failed"),
            window,
            cx,
        );
    }

    fn remove_member(&mut self, member: TeamMember, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("teams.remove_title"),
            t!("teams.remove_confirm", email = member.email),
            t!("common.remove"),
            true,
            move |window, cx| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let member = member.clone();
                view.update(cx, |this, cx| {
                    let (Some(api), Some(id)) = (this.model.read(cx).api.clone(), this.selected)
                    else {
                        return;
                    };
                    let user = member.user_id;
                    this.request(
                        async move {
                            api.delete(&format!("/api/v1/teams/{id}/members/{user}"))
                                .await
                                .map_err(api_error)
                        },
                        t!("teams.member_removed", email = member.email),
                        t!("teams.remove_failed"),
                        window,
                        cx,
                    );
                });
            },
        );
    }

    fn render_team_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let m = self.model.read(cx);
        let teams = m.teams.clone();
        let admin = m.is_admin();
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .children(teams.into_iter().enumerate().map(|(i, t)| {
                let active = self.selected == Some(t.id);
                let id = t.id;
                let role = t.role;
                h_flex()
                    .id(("team", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(if active { theme.primary } else { theme.border })
                    .bg(theme.secondary)
                    .hover(|s| s.bg(theme.secondary_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(id, window, cx)
                    }))
                    .child(
                        div()
                            .size(px(36.))
                            .flex_shrink_0()
                            .rounded(theme.radius)
                            .bg(theme::color_for(&t.name))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                ui::icon(IconName::Users)
                                    .size(px(18.))
                                    .text_color(gpui::white()),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_semibold()
                                    .text_sm()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(t.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(tn!("teams.members_count", t.member_count)),
                            ),
                    )
                    .child(match role {
                        Some(r) => ui::pill(role_label(r), role_color(r, cx)),
                        None if admin => ui::pill(t!("teams.server_admin_short"), theme.info),
                        None => ui::pill("—", theme.muted_foreground),
                    })
            }))
            .into_any_element()
    }

    /// Vaults of a team, as the current account sees them.
    fn team_vaults(&self, team: Id, cx: &gpui::App) -> Vec<VaultEntry> {
        let m = self.model.read(cx);
        m.current_account
            .and_then(|a| m.account(a))
            .map(|a| {
                a.vaults
                    .iter()
                    .filter(|v| v.vault.owner_team_id == Some(team))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// "Vaults" tab: the team's vaults, a new one (team owners and admins).
    fn render_vaults(
        &self,
        team: &Team,
        can_create: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let m = self.model.read(cx);
        let account = m.current_account;
        let supported = account
            .and_then(|a| m.account(a))
            .is_some_and(|a| a.vaults_supported());
        let vaults = self.team_vaults(team.id, cx);
        let model = self.model.clone();
        if !supported {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("accounts.no_vaults"))
                .into_any_element();
        }
        let team2 = team.clone();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("teams.vaults_hint")),
                    )
                    .when(can_create, |this| {
                        let model = model.clone();
                        this.child(
                            Button::new("team-new-vault")
                                .small()
                                .primary()
                                .icon(ui::icon(IconName::Plus))
                                .label(t!("vaults.new"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    if let Some(a) = account {
                                        vaults::open_create(
                                            model.clone(),
                                            a,
                                            Some(team2.clone()),
                                            window,
                                            cx,
                                        )
                                    }
                                }),
                        )
                    }),
            )
            .when(vaults.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("teams.no_vaults")),
                )
            })
            .children(vaults.into_iter().enumerate().map(|(i, v)| {
                let model = model.clone();
                let (acc, vid) = (v.account, v.id());
                h_flex()
                    .gap_3()
                    .p_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        ui::icon(vaults::vault_icon(&v.vault))
                            .size(px(18.))
                            .text_color(vaults::vault_color(&v.vault)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().font_semibold().text_sm().child(v.label()))
                                    .child(ui::pill(vaults::role_label(v.role()), theme.primary))
                                    .when(v.strict(), |this| {
                                        this.child(ui::pill(
                                            t!("vaults.strict_badge"),
                                            theme.warning,
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(vaults::counts_text(&v.vault)),
                            ),
                    )
                    .child(
                        Button::new(("team-vault-manage", i))
                            .small()
                            .icon(ui::icon(IconName::Settings))
                            .label(t!("vaults.manage"))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                vaults::open_manage(model.clone(), acc, vid, window, cx)
                            }),
                    )
            }))
            .into_any_element()
    }

    fn render_detail(&self, team: Team, cx: &mut Context<Self>) -> gpui::AnyElement {
        let m = self.model.read(cx);
        let me = m.me.as_ref().map(|u| u.id);
        let mine = m.team_role(&team);
        let is_member = team.role.is_some();
        let perms = Permissions::of(mine);
        let owners = self
            .members
            .iter()
            .filter(|x| x.role == TeamRole::Owner)
            .count();
        let last_owner = team.role == Some(TeamRole::Owner) && owners <= 1;
        let theme = cx.theme();
        let weak = cx.entity().downgrade();
        let (t_rename, t_delete, t_leave) = (team.clone(), team.clone(), team.clone());

        let header = h_flex()
            .gap_3()
            .items_center()
            .flex_wrap()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_xl().font_semibold().child(team.name.clone()))
                            .when_some(mine, |this, r| {
                                this.child(ui::pill(
                                    if team.role.is_none() {
                                        t!("teams.server_admin")
                                    } else {
                                        role_label(r)
                                    },
                                    role_color(r, cx),
                                ))
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{} · {}",
                                t!("teams.created_on", date = ui::format_ms(team.created_at)),
                                tn!("teams.members_count", team.member_count)
                            )),
                    ),
            )
            .when(perms.rename, |this| {
                this.child(
                    Button::new("team-rename")
                        .small()
                        .icon(ui::icon(IconName::Pencil))
                        .label(t!("teams.rename"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.team_name_dialog(Some(t_rename.clone()), window, cx)
                        })),
                )
            })
            .when(is_member, |this| {
                this.child(
                    Button::new("team-leave")
                        .small()
                        .icon(ui::icon(IconName::DoorOpen))
                        .label(t!("teams.leave"))
                        .disabled(last_owner)
                        .tooltip(if last_owner {
                            t!("teams.leave_last_owner")
                        } else {
                            t!("teams.leave_tooltip")
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.leave_team(t_leave.clone(), window, cx)
                        })),
                )
            })
            .when(perms.delete, |this| {
                this.child(
                    Button::new("team-delete")
                        .small()
                        .danger()
                        .icon(ui::icon(IconName::Trash))
                        .label(t!("teams.delete"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.delete_team(t_delete.clone(), window, cx)
                        })),
                )
            });

        let add = perms.add_members.then(|| {
            ui::field_with_hint(
                t!("teams.add_member"),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.add_email)))
                    .child(div().w(px(170.)).child(Select::new(&self.add_role)))
                    .child(
                        Button::new("add-member")
                            .primary()
                            .icon(ui::icon(IconName::UserPlus))
                            .label(t!("common.add"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.add_member(window, cx)
                            })),
                    ),
                t!("teams.add_member_hint"),
                cx,
            )
        });

        let rows = self.members.iter().enumerate().map(|(i, member)| {
            let is_me = Some(member.user_id) == me;
            let roles: Vec<TeamRole> = [TeamRole::Member, TeamRole::Admin, TeamRole::Owner]
                .into_iter()
                .filter(|r| perms.can_set_role(mine, member.role, *r))
                .collect();
            let removable = !is_me && perms.can_remove(mine, member.role);
            let show_menu = !is_me && (!roles.is_empty() || removable);
            let initials: String = member
                .name
                .split_whitespace()
                .chain(std::iter::once(member.email.as_str()))
                .filter_map(|w| w.chars().next())
                .take(2)
                .collect::<String>()
                .to_uppercase();
            let w = weak.clone();
            let member_menu = member.clone();
            h_flex()
                .id(("member", i))
                .px_3()
                .py_2()
                .gap_3()
                .items_center()
                .rounded(theme.radius)
                .hover(|s| s.bg(theme.secondary_hover))
                .child(
                    div()
                        .size(px(32.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme::color_for(&member.email))
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
                                .child(div().text_sm().font_medium().child(
                                    if member.name.is_empty() {
                                        member.email.clone()
                                    } else {
                                        member.name.clone()
                                    },
                                ))
                                .when(is_me, |this| {
                                    this.child(ui::pill(t!("teams.you"), theme.muted_foreground))
                                }),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                            "teams.member_since",
                            email = member.email,
                            date = ui::format_ms(member.added_at)
                        ))),
                )
                .child(ui::pill(
                    role_label(member.role),
                    role_color(member.role, cx),
                ))
                .when(show_menu, |this| {
                    this.child(
                        Button::new(("member-menu", i))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::EllipsisVertical))
                            .dropdown_menu(move |mut menu, _, _| {
                                for role in roles.clone() {
                                    let (w, m) = (w.clone(), member_menu.clone());
                                    menu = menu.item(
                                        PopupMenuItem::new(make_role_label(role))
                                            .icon(ui::icon(match role {
                                                TeamRole::Owner => IconName::Crown,
                                                TeamRole::Admin => IconName::UserCog,
                                                TeamRole::Member => IconName::User,
                                            }))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w.upgrade() {
                                                    let m = m.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.set_role(m, role, window, cx)
                                                    });
                                                }
                                            }),
                                    );
                                }
                                if removable {
                                    let (w, m) = (w.clone(), member_menu.clone());
                                    menu = menu.separator().item(
                                        PopupMenuItem::new(t!("teams.remove_title"))
                                            .icon(ui::icon(IconName::UserMinus))
                                            .on_click(move |_, window, cx| {
                                                if let Some(v) = w.upgrade() {
                                                    let m = m.clone();
                                                    v.update(cx, |v, cx| {
                                                        v.remove_member(m, window, cx)
                                                    });
                                                }
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                })
        });

        let tabs = TabBar::new("team-tabs")
            .selected_index(match self.tab {
                TeamTab::Members => 0,
                TeamTab::Vaults => 1,
            })
            .child(Tab::new().label(t!("teams.members")))
            .child(Tab::new().label(t!("teams.vaults")))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.tab = if *ix == 1 {
                    TeamTab::Vaults
                } else {
                    TeamTab::Members
                };
                cx.notify();
            }));
        if self.tab == TeamTab::Vaults {
            let vaults = self.render_vaults(&team, perms.rename, cx);
            return v_flex()
                .gap_5()
                .child(header)
                .child(tabs)
                .child(vaults)
                .into_any_element();
        }
        v_flex()
            .gap_5()
            .child(header)
            .child(tabs)
            .children(add)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .pb_1()
                            .child(div().font_semibold().child(t!("teams.members")))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(self.members.len().to_string()),
                            ),
                    )
                    .when_some(self.members_error.clone(), |this, e| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(t!("teams.members_error", error = e)),
                        )
                    })
                    .when(self.loading_members && self.members.is_empty(), |this| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("common.loading")),
                        )
                    })
                    .children(rows),
            )
            .into_any_element()
    }
}

/// Color of the role.
fn role_color(role: TeamRole, cx: &gpui::App) -> gpui::Hsla {
    let theme = cx.theme();
    match role {
        TeamRole::Owner => theme.warning,
        TeamRole::Admin => theme.primary,
        TeamRole::Member => theme.muted_foreground,
    }
}

impl Render for TeamsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let logged_in = self.model.read(cx).logged_in();
        let has_teams = !self.model.read(cx).teams.is_empty();
        let (list, detail) = if logged_in && has_teams {
            let list = self.render_team_list(cx);
            let detail = self.team(cx).map(|t| self.render_detail(t, cx));
            (Some(list), detail)
        } else {
            (None, None)
        };
        let theme = cx.theme();
        let body: gpui::AnyElement = if !logged_in {
            ui::empty_state(
                IconName::Users,
                t!("teams.no_server"),
                t!("teams.no_server_hint"),
                cx,
            )
            .into_any_element()
        } else if !has_teams {
            v_flex()
                .p_6()
                .child(ui::empty_state(
                    IconName::Users,
                    t!("teams.empty"),
                    t!("teams.empty_hint"),
                    cx,
                ))
                .into_any_element()
        } else {
            h_flex()
                .size_full()
                .items_start()
                .child(
                    div()
                        .w(px(320.))
                        .h_full()
                        .flex_shrink_0()
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            v_flex()
                                .id("team-list")
                                .size_full()
                                .overflow_y_scroll()
                                .p_4()
                                .children(list),
                        ),
                )
                .child(
                    div().flex_1().min_w_0().h_full().child(
                        v_flex()
                            .id("team-detail")
                            .size_full()
                            .overflow_y_scroll()
                            .p_6()
                            .children(detail),
                    ),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("teams.title"),
                t!("teams.subtitle"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("refresh-teams")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("common.refresh"))
                            .disabled(!logged_in)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.refresh(window, cx)
                            })),
                    )
                    .child(
                        Button::new("new-team")
                            .primary()
                            .icon(ui::icon(IconName::Plus))
                            .label(t!("teams.new_team"))
                            .disabled(!logged_in)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.new_team(window, cx)
                            })),
                    ),
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TeamRole::*;

    #[test]
    fn members_can_only_look_and_leave() {
        let p = Permissions::of(Some(Member));
        assert!(!p.rename && !p.delete && !p.add_members && !p.grant_owner);
        assert!(!p.can_set_role(Some(Member), Member, Admin));
        assert!(!p.can_remove(Some(Member), Member));
        let none = Permissions::of(None);
        assert!(!none.add_members);
    }

    #[test]
    fn admins_manage_members_but_not_owners() {
        let mine = Some(Admin);
        let p = Permissions::of(mine);
        assert!(p.rename && p.add_members && !p.delete && !p.grant_owner);
        assert!(p.can_set_role(mine, Member, Admin));
        assert!(p.can_set_role(mine, Admin, Member));
        assert!(!p.can_set_role(mine, Member, Owner));
        assert!(!p.can_set_role(mine, Owner, Admin));
        assert!(p.can_remove(mine, Member));
        assert!(!p.can_remove(mine, Owner));
        // Same role: not a change.
        assert!(!p.can_set_role(mine, Admin, Admin));
    }

    #[test]
    fn owners_can_do_everything() {
        let mine = Some(Owner);
        let p = Permissions::of(mine);
        assert!(p.rename && p.add_members && p.delete && p.grant_owner);
        assert!(p.can_set_role(mine, Owner, Admin));
        assert!(p.can_set_role(mine, Member, Owner));
        assert!(p.can_remove(mine, Owner));
    }
}
