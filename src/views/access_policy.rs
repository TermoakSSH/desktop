//! The access policy of a vault, group or host (vault Admins,
//! docs-next/TEAMS-SECURITY.md §5.1): direct access or approval, who
//! approves, the longest window, how long a request waits, self-approval
//! (single-person teams), whether Admins need a request too, mandatory
//! recording and certificate overrides for those hosts.
//!
//! A host without its own policy follows its nearest group's, else the
//! vault's: the editor says which one applies now.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_client::ApiClient;
use termoak_core::Id;
use termoak_core::access::{AccessMode, AccessPolicy, AccessPolicyRecord, AccessTargetKind};
use termoak_core::model::{VaultMember, VaultPrincipal, VaultRole};

use super::cert_fields::{CertFields, CertFieldsKind, CertValues};
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// What the editor is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyTarget {
    pub account: Id,
    pub vault: Id,
    pub kind: AccessTargetKind,
    pub id: Id,
    pub label: String,
    /// Groups from the target up (a host's group first), to say what it
    /// inherits.
    pub parents: Vec<(Id, String)>,
}

/// The target for a vault, group or host of an account's vault (its label
/// and groups up, for what it inherits).
pub fn target_in(
    model: &Entity<AppModel>,
    account: Id,
    vault: Id,
    kind: AccessTargetKind,
    id: Id,
    cx: &App,
) -> PolicyTarget {
    let m = model.read(cx);
    let chain = |start: Option<Id>| -> Vec<(Id, String)> {
        let mut out = Vec::new();
        let mut next = start;
        while let Some(g) = next {
            let Some(rec) = m.groups.iter().find(|x| x.data.id == g) else {
                break;
            };
            if out.iter().any(|(x, _)| *x == g) || out.len() > 16 {
                break;
            }
            out.push((g, rec.data.name.clone()));
            next = rec.data.parent_id;
        }
        out
    };
    let (label, parents) = match kind {
        AccessTargetKind::Vault => (
            m.vault_entry(account, vault)
                .map(|v| v.label())
                .unwrap_or_default(),
            Vec::new(),
        ),
        AccessTargetKind::Group => {
            let g = m.groups.iter().find(|g| g.data.id == id);
            (
                g.map(|g| g.data.name.clone()).unwrap_or_default(),
                chain(g.and_then(|g| g.data.parent_id)),
            )
        }
        _ => {
            let h = m.host_record(id);
            (
                h.map(|h| h.data.label.clone()).unwrap_or_default(),
                chain(h.and_then(|h| h.data.group_id)),
            )
        }
    };
    PolicyTarget {
        account,
        vault,
        kind,
        id,
        label,
        parents,
    }
}

/// The policy that applies to a target without its own: its nearest
/// group's, else the vault's (`None`: direct access).
pub fn inherited<'a>(
    target: &PolicyTarget,
    policies: &'a [AccessPolicyRecord],
) -> Option<(&'a AccessPolicyRecord, String)> {
    for (g, name) in &target.parents {
        if let Some(p) = policies
            .iter()
            .find(|p| p.target_kind == AccessTargetKind::Group && p.target_id == *g)
        {
            return Some((p, name.clone()));
        }
    }
    if target.kind != AccessTargetKind::Vault {
        if let Some(p) = policies
            .iter()
            .find(|p| p.target_kind == AccessTargetKind::Vault)
        {
            return Some((p, t!("access_policy.the_vault").to_string()));
        }
    }
    None
}

/// The policy from the form (`Err`: what is wrong, from the core's checks).
#[allow(clippy::too_many_arguments)]
pub fn policy_from_form(
    mode: AccessMode,
    approvers: Vec<Id>,
    max: &str,
    ttl: &str,
    self_approval: bool,
    managers: bool,
    record: bool,
    cert: &CertValues,
) -> Result<AccessPolicy, String> {
    let num = |s: &str, err: &str| -> Result<u32, String> {
        s.trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| t!(err).to_string())
    };
    let p = AccessPolicy {
        mode,
        approvers,
        max_minutes: num(max, "access_policy.error.max")?,
        request_ttl_minutes: num(ttl, "access_policy.error.ttl")?,
        allow_self_approval: self_approval,
        require_for_managers: managers,
        principals: cert.principals.clone(),
        cert_ttl_minutes: cert.ttl,
        extensions: cert.extensions.clone(),
        source_address: cert.source_address.clone(),
        force_command: cert.force_command.clone(),
        record_sessions: record,
    };
    p.validate().map_err(|e| e.to_string())?;
    Ok(p)
}

pub struct AccessPolicyEditor {
    model: Entity<AppModel>,
    target: PolicyTarget,
    policies: Vec<AccessPolicyRecord>,
    own: Option<AccessPolicyRecord>,
    members: Vec<VaultMember>,
    loaded: bool,
    error: Option<String>,
    mode: ChoiceState<AccessMode>,
    approvers: Vec<Id>,
    max: Entity<InputState>,
    ttl: Entity<InputState>,
    self_approval: bool,
    managers: bool,
    record: bool,
    cert: Option<Entity<CertFields>>,
    advanced: bool,
    busy: bool,
}

/// Opens the editor in a dialog.
pub fn open(model: Entity<AppModel>, target: PolicyTarget, window: &mut Window, cx: &mut App) {
    let title = match target.kind {
        AccessTargetKind::Vault => t!("access_policy.title_vault", name = target.label.clone()),
        AccessTargetKind::Group => t!("access_policy.title_group", name = target.label.clone()),
        _ => t!("access_policy.title_host", name = target.label.clone()),
    };
    let view = cx.new(|cx| AccessPolicyEditor::new(model, target, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(title.clone()).w(px(600.)).child(view.clone())
    });
}

impl AccessPolicyEditor {
    pub fn new(
        model: Entity<AppModel>,
        target: PolicyTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mode = ui::choice_state(
            vec![
                Choice::new(t!("access_policy.mode.direct"), AccessMode::Direct),
                Choice::new(t!("access_policy.mode.approval"), AccessMode::Approval),
            ],
            Some(&AccessMode::Approval),
            window,
            cx,
        );
        let max = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("60")
                .default_value("60")
        });
        let ttl = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("240")
                .default_value("240")
        });
        let mut this = Self {
            model,
            target,
            policies: Vec::new(),
            own: None,
            members: Vec::new(),
            loaded: false,
            error: None,
            mode,
            approvers: Vec::new(),
            max,
            ttl,
            self_approval: false,
            managers: false,
            record: false,
            cert: None,
            advanced: false,
            busy: false,
        };
        this.load(window, cx);
        this
    }

    fn api(&self, cx: &App) -> Option<ApiClient> {
        self.model.read(cx).api_of(Some(self.target.account))
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            self.error = Some(t!("vaults.not_signed_in").to_string());
            return;
        };
        let vault = self.target.vault;
        runtime::run_in(
            cx,
            window,
            async move {
                let policies: Vec<AccessPolicyRecord> = api
                    .get(&format!("/api/v1/vaults/{vault}/access-policies"))
                    .await?;
                let members = api.vault_members(vault).await.unwrap_or_default();
                Ok::<_, termoak_client::ClientError>((policies, members))
            },
            |this, res, window, cx| {
                match res {
                    Ok((policies, members)) => {
                        this.members = members;
                        this.set_policies(policies, window, cx);
                    }
                    Err(e) => this.error = Some(e),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn set_policies(
        &mut self,
        policies: Vec<AccessPolicyRecord>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let own = policies
            .iter()
            .find(|p| p.target_kind == self.target.kind && p.target_id == self.target.id)
            .cloned();
        let p = own
            .as_ref()
            .map(|r| r.policy.clone())
            .unwrap_or(AccessPolicy {
                mode: AccessMode::Approval,
                ..Default::default()
            });
        self.mode
            .update(cx, |s, cx| s.set_selected_value(&p.mode, window, cx));
        self.max.update(cx, |i, cx| {
            i.set_value(p.max_minutes.to_string(), window, cx)
        });
        self.ttl.update(cx, |i, cx| {
            i.set_value(p.request_ttl_minutes.to_string(), window, cx)
        });
        self.approvers = p.approvers.clone();
        self.self_approval = p.allow_self_approval;
        self.managers = p.require_for_managers;
        self.record = p.record_sessions;
        self.advanced = p.principals.is_some()
            || p.cert_ttl_minutes.is_some()
            || p.extensions.is_some()
            || p.source_address.is_some()
            || p.force_command.is_some();
        let (email, user_id) = {
            let m = self.model.read(cx);
            let a = m.account(self.target.account);
            (
                a.map(|a| a.info.email.clone()).unwrap_or_default(),
                a.and_then(|a| a.info.user_id).unwrap_or_default(),
            )
        };
        let initial = CertValues {
            principals: p.principals.clone(),
            ttl: p.cert_ttl_minutes,
            max_ttl: None,
            extensions: p.extensions.clone(),
            source_address: p.source_address.clone(),
            force_command: p.force_command.clone(),
        };
        self.cert = Some(cx.new(|cx| {
            CertFields::new(
                CertFieldsKind::Override,
                initial,
                email,
                user_id,
                window,
                cx,
            )
        }));
        self.own = own;
        self.policies = policies;
    }

    fn after_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let account = self.target.account;
        self.model.update(cx, |m, cx| m.sync_account(account, cx));
        let book = crate::access::AccessBook::global(cx);
        book.update(cx, |b, cx| b.forget_hosts(account, cx));
        window.close_dialog(cx);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let cert = match self.cert.as_ref().map(|c| c.read(cx).values(cx)) {
            Some(Ok(v)) if self.advanced => v,
            Some(Err(e)) if self.advanced => {
                ui::error(window, cx, e);
                return;
            }
            _ => CertValues::default(),
        };
        let policy = match policy_from_form(
            ui::chosen(&self.mode, cx).unwrap_or(AccessMode::Approval),
            self.approvers.clone(),
            self.max.read(cx).value().as_ref(),
            self.ttl.read(cx).value().as_ref(),
            self.self_approval,
            self.managers,
            self.record,
            &cert,
        ) {
            Ok(p) => p,
            Err(e) => {
                ui::error(window, cx, e);
                return;
            }
        };
        let body = json!({
            "target_kind": self.target.kind,
            "target_id": self.target.id,
            "policy": policy,
        });
        let vault = self.target.vault;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.put::<Value>(&format!("/api/v1/vaults/{vault}/access-policies"), &body)
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(_) => {
                        ui::success(window, cx, t!("access_policy.saved"));
                        this.after_change(window, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(api), Some(own)) = (self.api(cx), self.own.clone()) else {
            return;
        };
        let vault = self.target.vault;
        self.busy = true;
        runtime::run_in(
            cx,
            window,
            async move {
                api.delete(&format!(
                    "/api/v1/vaults/{vault}/access-policies/{}",
                    own.id
                ))
                .await
                .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(_) => {
                        ui::success(window, cx, t!("access_policy.removed"));
                        this.after_change(window, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn render_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let approval = ui::chosen(&self.mode, cx) == Some(AccessMode::Approval);
        let now_text: SharedString = match (&self.own, inherited(&self.target, &self.policies)) {
            (Some(_), _) => t!("access_policy.own"),
            (None, Some((p, from))) => {
                if p.policy.mode.requires_approval() {
                    t!("access_policy.inherits_approval", from = from)
                } else {
                    t!("access_policy.inherits_direct", from = from)
                }
            }
            (None, None) => t!("access_policy.none"),
        };
        // People who can approve: users with access (team grants are not
        // listed one by one).
        let users: Vec<(Id, String, VaultRole)> = self
            .members
            .iter()
            .filter_map(|m| match &m.principal {
                VaultPrincipal::User { id, email, name } => Some((
                    *id,
                    if name.trim().is_empty() {
                        email.clone()
                    } else {
                        format!("{name} ({email})")
                    },
                    m.role,
                )),
                _ => None,
            })
            .collect();
        let switch = |id: &'static str,
                      label: SharedString,
                      hint: SharedString,
                      on: bool,
                      set: fn(&mut Self, bool),
                      cx: &mut Context<Self>| {
            v_flex()
                .gap_0p5()
                .child(
                    Switch::new(id)
                        .label(label)
                        .checked(on)
                        .on_click(cx.listener(move |this, v: &bool, _, cx| {
                            set(this, *v);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                )
        };
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(muted).child(now_text))
            .child(ui::field_with_hint(
                t!("access_policy.mode.label"),
                Select::new(&self.mode),
                t!("access_policy.mode.hint"),
                cx,
            ))
            .when(approval, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(t!("access_policy.approvers")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("access_policy.approvers_hint")),
                        )
                        .child(
                            v_flex()
                                .id("approvers")
                                .max_h(px(140.))
                                .overflow_y_scrollbar()
                                .gap_1()
                                .children(users.into_iter().enumerate().map(
                                    |(i, (id, label, role))| {
                                        Checkbox::new(("approver", i))
                                            .label(format!(
                                                "{label} · {}",
                                                super::vaults::role_label(role)
                                            ))
                                            .checked(self.approvers.contains(&id))
                                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                                this.approvers.retain(|a| *a != id);
                                                if *v {
                                                    this.approvers.push(id);
                                                }
                                                cx.notify();
                                            }))
                                    },
                                )),
                        ),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .child(div().flex_1().child(ui::field_with_hint(
                            t!("access_policy.max"),
                            Input::new(&self.max),
                            t!("access_policy.max_hint"),
                            cx,
                        )))
                        .child(div().flex_1().child(ui::field_with_hint(
                            t!("access_policy.ttl"),
                            Input::new(&self.ttl),
                            t!("access_policy.ttl_hint"),
                            cx,
                        ))),
                )
                .child(switch(
                    "ap-self",
                    t!("access_policy.self_approval"),
                    t!("access_policy.self_approval_hint"),
                    self.self_approval,
                    |t, v| t.self_approval = v,
                    cx,
                ))
                .child(switch(
                    "ap-managers",
                    t!("access_policy.managers"),
                    t!("access_policy.managers_hint"),
                    self.managers,
                    |t, v| t.managers = v,
                    cx,
                ))
            })
            .child(switch(
                "ap-record",
                t!("access_policy.record"),
                t!("access_policy.record_hint"),
                self.record,
                |t, v| t.record = v,
                cx,
            ))
            .child(
                Switch::new("ap-advanced")
                    .label(t!("access_policy.cert_overrides"))
                    .checked(self.advanced)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.advanced = *v;
                        cx.notify();
                    })),
            )
            .when(self.advanced, |this| this.children(self.cert.clone()))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(self.own.is_some(), |this| {
                        this.child(
                            Button::new("ap-remove")
                                .danger()
                                .icon(ui::icon(IconName::Trash))
                                .label(t!("access_policy.remove"))
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.remove(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("ap-save")
                            .primary()
                            .icon(ui::icon(IconName::Save))
                            .label(t!("common.save"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for AccessPolicyEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = if let Some(e) = &self.error {
            div()
                .text_sm()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element()
        } else if !self.loaded {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("common.loading"))
                .into_any_element()
        } else {
            self.render_form(cx)
        };
        v_flex()
            .id("access-policy")
            .max_h(px(560.))
            .overflow_y_scrollbar()
            .pr_2()
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: AccessTargetKind, id: Id, mode: AccessMode) -> AccessPolicyRecord {
        AccessPolicyRecord {
            id: termoak_core::new_id(),
            vault_id: Id::nil(),
            target_kind: kind,
            target_id: id,
            policy: AccessPolicy {
                mode,
                ..Default::default()
            },
            updated_by: Id::nil(),
            updated_at: 0,
        }
    }

    #[test]
    fn nearest_group_then_vault() {
        let (g1, g2, host) = (
            termoak_core::new_id(),
            termoak_core::new_id(),
            termoak_core::new_id(),
        );
        let target = PolicyTarget {
            account: Id::nil(),
            vault: Id::nil(),
            kind: AccessTargetKind::Host,
            id: host,
            label: "db".into(),
            parents: vec![(g1, "Prod".into()), (g2, "All".into())],
        };
        let vault = record(AccessTargetKind::Vault, Id::nil(), AccessMode::Direct);
        let outer = record(AccessTargetKind::Group, g2, AccessMode::Approval);
        assert_eq!(inherited(&target, &[]), None);
        assert_eq!(
            inherited(&target, std::slice::from_ref(&vault)).unwrap().0,
            &vault
        );
        let both = [vault.clone(), outer.clone()];
        let (p, from) = inherited(&target, &both).unwrap();
        assert_eq!(p, &outer);
        assert_eq!(from, "All");
    }

    #[test]
    fn form_values_are_checked_by_the_core() {
        let ok = policy_from_form(
            AccessMode::Approval,
            vec![],
            "120",
            "60",
            false,
            true,
            true,
            &CertValues::default(),
        )
        .unwrap();
        assert_eq!(ok.max_minutes, 120);
        assert!(ok.require_for_managers && ok.record_sessions);
        assert!(
            policy_from_form(
                AccessMode::Approval,
                vec![],
                "0",
                "60",
                false,
                false,
                false,
                &CertValues::default()
            )
            .is_err()
        );
        // Requests must wait at least 5 minutes.
        assert!(
            policy_from_form(
                AccessMode::Approval,
                vec![],
                "60",
                "2",
                false,
                false,
                false,
                &CertValues::default()
            )
            .is_err()
        );
        let bad = CertValues {
            source_address: Some("not an ip".into()),
            ..Default::default()
        };
        assert!(
            policy_from_form(
                AccessMode::Direct,
                vec![],
                "60",
                "60",
                false,
                false,
                false,
                &bad
            )
            .is_err()
        );
    }
}
