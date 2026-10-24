//! Teams → Security (docs-next/TEAMS-SECURITY.md §4, §6): the team's SSH
//! CA, its policy (two-step verification with a grace period, revealing
//! secrets, the AI and its approval level, private networks, mandatory
//! recording, certificate defaults) and, for owners and admins, who lacks
//! two-step verification and each member's certificate principal.
//!
//! Members see the CA and a summary of the policy.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use serde::Deserialize;
use serde_json::{Value, json};
use termoak_client::ApiClient;
use termoak_core::Id;
use termoak_core::access::{AiApproval, TeamPolicy};

use super::ca_panel::{CaPanel, CaScope};
use super::cert_fields::{CertFields, CertFieldsKind, CertValues};
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// A member in `GET /teams/{id}/compliance`.
#[derive(Debug, Clone, Deserialize)]
pub struct ComplianceMember {
    pub user_id: Id,
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub role: String,
    pub totp_enabled: bool,
    pub deadline: Option<i64>,
    #[serde(default)]
    pub blocked: bool,
    pub principal: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Compliance {
    #[serde(default)]
    members: Vec<ComplianceMember>,
}

/// State of a member's two-step verification for the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoFaState {
    On,
    /// Missing, with the end of the grace period.
    Grace(i64),
    Blocked,
    /// Missing, the team does not require it.
    Off,
}

pub fn twofa_state(m: &ComplianceMember) -> TwoFaState {
    if m.totp_enabled {
        TwoFaState::On
    } else if m.blocked {
        TwoFaState::Blocked
    } else if let Some(d) = m.deadline {
        TwoFaState::Grace(d)
    } else {
        TwoFaState::Off
    }
}

/// The policy with the form's values (`Err`: what is wrong).
pub fn policy_from_form(
    base: &TeamPolicy,
    form: &PolicyForm,
    cert: &CertValues,
) -> Result<TeamPolicy, String> {
    let mut p = base.clone();
    p.require_2fa = form.require_2fa;
    p.grace_days = form
        .grace_days
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|d| *d <= 90)
        .ok_or_else(|| t!("team_security.error.grace").to_string())?;
    p.allow_reveal = form.allow_reveal;
    p.ai.allowed = form.ai_allowed;
    p.ai.approval = form.ai_approval;
    p.networks_allowed = form.networks_allowed;
    p.record_sessions = form.record_sessions;
    if let Some(v) = &cert.principals {
        p.ca.principals = v.clone();
    }
    if let Some(v) = cert.ttl {
        p.ca.default_ttl_minutes = v;
    }
    if let Some(v) = cert.max_ttl {
        p.ca.max_ttl_minutes = v;
    }
    if let Some(v) = &cert.extensions {
        p.ca.extensions = v.clone();
    }
    p.ca.source_address = cert.source_address.clone();
    p.validate().map_err(|e| e.to_string())?;
    Ok(p)
}

/// The switches of the policy form.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyForm {
    pub require_2fa: bool,
    pub grace_days: String,
    pub allow_reveal: bool,
    pub ai_allowed: bool,
    pub ai_approval: AiApproval,
    pub networks_allowed: bool,
    pub record_sessions: bool,
}

impl PolicyForm {
    fn of(p: &TeamPolicy) -> Self {
        Self {
            require_2fa: p.require_2fa,
            grace_days: p.grace_days.to_string(),
            allow_reveal: p.allow_reveal,
            ai_allowed: p.ai.allowed,
            ai_approval: p.ai.approval,
            networks_allowed: p.networks_allowed,
            record_sessions: p.record_sessions,
        }
    }
}

pub struct TeamSecurity {
    model: Entity<AppModel>,
    team: Id,
    manage: bool,
    ca: Entity<CaPanel>,
    policy: Option<TeamPolicy>,
    form: Option<PolicyForm>,
    grace: Entity<InputState>,
    ai_approval: ChoiceState<AiApproval>,
    cert: Option<Entity<CertFields>>,
    policy_error: Option<String>,
    compliance: Vec<ComplianceMember>,
    compliance_error: Option<String>,
    busy: bool,
}

impl TeamSecurity {
    pub fn new(
        model: Entity<AppModel>,
        team: Id,
        team_name: String,
        manage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ca = {
            let model = model.clone();
            cx.new(|cx| CaPanel::new(model, None, CaScope::Team(team, team_name), window, cx))
        };
        let grace = cx.new(|cx| InputState::new(window, cx).placeholder("7"));
        let ai_approval = ui::choice_state(
            vec![
                Choice::new(t!("team_security.ai.approval_default"), AiApproval::Default),
                Choice::new(t!("team_security.ai.approval_always"), AiApproval::Always),
            ],
            Some(&AiApproval::Default),
            window,
            cx,
        );
        let mut this = Self {
            model,
            team,
            manage,
            ca,
            policy: None,
            form: None,
            grace,
            ai_approval,
            cert: None,
            policy_error: None,
            compliance: Vec::new(),
            compliance_error: None,
            busy: false,
        };
        this.load(window, cx);
        this
    }

    fn api(&self, cx: &App) -> Option<ApiClient> {
        self.model.read(cx).api.clone()
    }

    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let team = self.team;
        let api2 = api.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<TeamPolicy>(&format!("/api/v1/teams/{team}/policy"))
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                match res {
                    Ok(p) => this.set_policy(p, window, cx),
                    Err(e) => this.policy_error = Some(e),
                }
                cx.notify();
            },
        );
        if self.manage {
            runtime::run_in(
                cx,
                window,
                async move {
                    api2.get::<Compliance>(&format!("/api/v1/teams/{team}/compliance"))
                        .await
                        .map_err(api_error)
                },
                |this, res, _, cx| {
                    match res {
                        Ok(c) => {
                            this.compliance = c.members;
                            this.compliance_error = None;
                        }
                        Err(e) => this.compliance_error = Some(e),
                    }
                    cx.notify();
                },
            );
        }
    }

    fn set_policy(&mut self, p: TeamPolicy, window: &mut Window, cx: &mut Context<Self>) {
        let form = PolicyForm::of(&p);
        self.grace
            .update(cx, |i, cx| i.set_value(form.grace_days.clone(), window, cx));
        self.ai_approval.update(cx, |s, cx| {
            s.set_selected_value(&form.ai_approval, window, cx)
        });
        let (email, user_id) = {
            let m = self.model.read(cx);
            (
                m.me.as_ref()
                    .map(|u| u.email.clone())
                    .unwrap_or_else(|| "ana@example.com".into()),
                m.me.as_ref().map(|u| u.id).unwrap_or_default(),
            )
        };
        let initial = CertValues {
            principals: Some(p.ca.principals.clone()),
            ttl: Some(p.ca.default_ttl_minutes),
            max_ttl: Some(p.ca.max_ttl_minutes),
            extensions: Some(p.ca.extensions.clone()),
            source_address: p.ca.source_address.clone(),
            force_command: None,
        };
        self.cert =
            Some(cx.new(|cx| {
                CertFields::new(CertFieldsKind::Team, initial, email, user_id, window, cx)
            }));
        self.form = Some(form);
        self.policy = Some(p);
        self.policy_error = None;
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(api), Some(base), Some(form), Some(cert)) = (
            self.api(cx),
            self.policy.clone(),
            self.form.clone(),
            self.cert.clone(),
        ) else {
            return;
        };
        let mut form = form;
        form.grace_days = self.grace.read(cx).value().to_string();
        form.ai_approval = ui::chosen(&self.ai_approval, cx).unwrap_or_default();
        let cert = match cert.read(cx).values(cx) {
            Ok(v) => v,
            Err(e) => {
                ui::error(window, cx, e);
                return;
            }
        };
        let policy = match policy_from_form(&base, &form, &cert) {
            Ok(p) => p,
            Err(e) => {
                ui::error(window, cx, e);
                return;
            }
        };
        let team = self.team;
        self.busy = true;
        cx.notify();
        let body = serde_json::to_value(&policy).unwrap_or(Value::Null);
        runtime::run_in(
            cx,
            window,
            async move {
                api.put::<TeamPolicy>(&format!("/api/v1/teams/{team}/policy"), &body)
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(p) => {
                        ui::success(window, cx, t!("team_security.saved"));
                        this.set_policy(p, window, cx);
                        this.load(window, cx);
                        this.model.update(cx, |m, cx| m.sync_now(cx));
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn edit_principal(&mut self, m: ComplianceMember, window: &mut Window, cx: &mut Context<Self>) {
        let default = termoak_core::access::member_principal(&m.email);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(default.clone())
                .default_value(m.principal.clone().unwrap_or_default())
        });
        let weak = cx.entity().downgrade();
        let i2 = input.clone();
        let user = m.user_id;
        ui::open_form_dialog(
            window,
            cx,
            t!("team_security.principal.title", email = m.email.clone()),
            t!("common.save"),
            460.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!(
                        "team_security.principal.message",
                        default = default.clone()
                    )))
                    .child(ui::field(
                        t!("team_security.principal.label"),
                        Input::new(&input),
                        cx,
                    ))
                    .into_any_element()
            },
            move |window, cx| {
                let value = i2.read(cx).value().trim().to_string();
                if !value.is_empty() && termoak_core::access::validate_principal(&value).is_err() {
                    ui::error(window, cx, t!("team_security.principal.invalid"));
                    return false;
                }
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |this, cx| {
                        let Some(api) = this.api(cx) else {
                            return;
                        };
                        let team = this.team;
                        runtime::run_in(
                            cx,
                            window,
                            async move {
                                api.patch::<Value>(
                                    &format!("/api/v1/teams/{team}/members/{user}"),
                                    &json!({"principal": value}),
                                )
                                .await
                                .map_err(api_error)
                            },
                            |this, res, window, cx| match res {
                                Ok(_) => {
                                    ui::success(window, cx, t!("team_security.principal.saved"));
                                    this.load(window, cx);
                                }
                                Err(e) => ui::error(window, cx, e),
                            },
                        );
                    });
                }
                true
            },
        );
    }

    fn render_policy(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (Some(form), Some(policy)) = (&self.form, &self.policy) else {
            return div()
                .text_sm()
                .text_color(if self.policy_error.is_some() {
                    theme.danger
                } else {
                    muted
                })
                .child(
                    self.policy_error
                        .clone()
                        .unwrap_or_else(|| t!("common.loading").to_string()),
                )
                .into_any_element();
        };
        if !self.manage {
            let yes_no = |b: bool| if b { t!("common.yes") } else { t!("common.no") };
            let row = |label: gpui::SharedString, value: gpui::SharedString| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().flex_1().text_color(muted).child(label))
                    .child(div().font_medium().child(value))
            };
            return v_flex()
                .gap_1()
                .child(row(
                    t!("team_security.require_2fa"),
                    yes_no(policy.require_2fa),
                ))
                .child(row(
                    t!("team_security.allow_reveal"),
                    yes_no(policy.allow_reveal),
                ))
                .child(row(
                    t!("team_security.ai.allowed"),
                    yes_no(policy.ai.allowed),
                ))
                .child(row(
                    t!("team_security.networks"),
                    yes_no(policy.networks_allowed),
                ))
                .child(row(
                    t!("team_security.record"),
                    yes_no(policy.record_sessions),
                ))
                .child(row(
                    t!("team_security.ttl_summary"),
                    t!(
                        "team_security.ttl_value",
                        default = policy.ca.default_ttl_minutes,
                        max = policy.ca.max_ttl_minutes
                    ),
                ))
                .into_any_element();
        }
        let switch = |id: &'static str,
                      label: gpui::SharedString,
                      hint: gpui::SharedString,
                      on: bool,
                      set: fn(&mut PolicyForm, bool),
                      cx: &mut Context<Self>| {
            v_flex()
                .gap_0p5()
                .child(
                    Switch::new(id)
                        .label(label)
                        .checked(on)
                        .on_click(cx.listener(move |this, v: &bool, _, cx| {
                            if let Some(f) = this.form.as_mut() {
                                set(f, *v);
                            }
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
            .child(switch(
                "pol-2fa",
                t!("team_security.require_2fa"),
                t!("team_security.require_2fa_hint"),
                form.require_2fa,
                |f, v| f.require_2fa = v,
                cx,
            ))
            .when(form.require_2fa, |this| {
                this.child(div().w(px(200.)).child(ui::field(
                    t!("team_security.grace_days"),
                    Input::new(&self.grace),
                    cx,
                )))
            })
            .child(switch(
                "pol-reveal",
                t!("team_security.allow_reveal"),
                t!("team_security.allow_reveal_hint"),
                form.allow_reveal,
                |f, v| f.allow_reveal = v,
                cx,
            ))
            .child(switch(
                "pol-ai",
                t!("team_security.ai.allowed"),
                t!("team_security.ai.allowed_hint"),
                form.ai_allowed,
                |f, v| f.ai_allowed = v,
                cx,
            ))
            .when(form.ai_allowed, |this| {
                this.child(div().w(px(320.)).child(ui::field(
                    t!("team_security.ai.approval"),
                    Select::new(&self.ai_approval),
                    cx,
                )))
            })
            .child(switch(
                "pol-networks",
                t!("team_security.networks"),
                t!("team_security.networks_hint"),
                form.networks_allowed,
                |f, v| f.networks_allowed = v,
                cx,
            ))
            .child(switch(
                "pol-record",
                t!("team_security.record"),
                t!("team_security.record_hint"),
                form.record_sessions,
                |f, v| f.record_sessions = v,
                cx,
            ))
            .child(
                div()
                    .pt_2()
                    .text_sm()
                    .font_semibold()
                    .child(t!("team_security.cert_defaults")),
            )
            .children(self.cert.clone())
            .child(
                h_flex().justify_end().child(
                    Button::new("pol-save")
                        .primary()
                        .icon(ui::icon(IconName::Save))
                        .label(t!("team_security.save"))
                        .loading(self.busy)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.save(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_compliance(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let missing = self.compliance.iter().filter(|m| !m.totp_enabled).count();
        let weak = cx.entity().downgrade();
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(muted).child(if missing == 0 {
                t!("team_security.compliance.all")
            } else {
                tn!("team_security.compliance.missing", missing)
            }))
            .when_some(self.compliance_error.clone(), |this, e| {
                this.child(div().text_sm().text_color(theme.danger).child(e))
            })
            .children(self.compliance.iter().enumerate().map(|(i, m)| {
                let state = twofa_state(m);
                let (label, color) = match state {
                    TwoFaState::On => (t!("team_security.compliance.on"), theme.success),
                    TwoFaState::Grace(d) => (
                        t!("team_security.compliance.until", date = ui::format_ms(d)),
                        theme.warning,
                    ),
                    TwoFaState::Blocked => (t!("team_security.compliance.blocked"), theme.danger),
                    TwoFaState::Off => (t!("team_security.compliance.off"), muted),
                };
                let principal = m
                    .principal
                    .clone()
                    .unwrap_or_else(|| termoak_core::access::member_principal(&m.email));
                let member = m.clone();
                let w = weak.clone();
                h_flex()
                    .gap_2()
                    .p_2()
                    .items_center()
                    .rounded(theme.radius)
                    .bg(theme.secondary)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_medium().child(
                                if m.name.trim().is_empty() {
                                    m.email.clone()
                                } else {
                                    m.name.clone()
                                },
                            ))
                            .child(div().text_xs().text_color(muted).child(t!(
                                "team_security.compliance.principal",
                                email = m.email.clone(),
                                principal = principal
                            ))),
                    )
                    .child(ui::pill(label, color))
                    .child(
                        Button::new(("principal-edit", i))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::Pencil))
                            .tooltip(t!("team_security.principal.edit"))
                            .on_click(move |_, window, cx| {
                                if let Some(v) = w.upgrade() {
                                    let member = member.clone();
                                    v.update(cx, |v, cx| v.edit_principal(member, window, cx));
                                }
                            }),
                    )
            }))
            .into_any_element()
    }
}

impl Render for TeamSecurity {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section = |title: gpui::SharedString, body: gpui::AnyElement, cx: &App| {
            ui::card(cx)
                .p_4()
                .gap_3()
                .child(div().font_semibold().child(title))
                .child(body)
        };
        let policy = self.render_policy(cx);
        let compliance = self.manage.then(|| self.render_compliance(cx));
        v_flex()
            .gap_4()
            .child(ui::card(cx).p_4().child(self.ca.clone()))
            .child(section(t!("team_security.policy_title"), policy, cx))
            .children(compliance.map(|c| section(t!("team_security.compliance.title"), c, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(totp: bool, deadline: Option<i64>, blocked: bool) -> ComplianceMember {
        ComplianceMember {
            user_id: Id::nil(),
            email: "a@b.c".into(),
            name: String::new(),
            role: "member".into(),
            totp_enabled: totp,
            deadline,
            blocked,
            principal: None,
        }
    }

    #[test]
    fn compliance_states() {
        assert_eq!(twofa_state(&member(true, None, false)), TwoFaState::On);
        assert_eq!(
            twofa_state(&member(false, Some(5), false)),
            TwoFaState::Grace(5)
        );
        assert_eq!(
            twofa_state(&member(false, Some(5), true)),
            TwoFaState::Blocked
        );
        assert_eq!(twofa_state(&member(false, None, false)), TwoFaState::Off);
    }

    #[test]
    fn policy_form_round_trip_and_checks() {
        let base = TeamPolicy::default();
        let mut form = PolicyForm::of(&base);
        form.require_2fa = true;
        form.grace_days = "14".into();
        form.allow_reveal = false;
        form.ai_approval = AiApproval::Always;
        let cert = CertValues {
            principals: Some(vec!["{login}".into(), "prod-{member}".into()]),
            ttl: Some(30),
            max_ttl: Some(120),
            extensions: Some(vec!["permit-pty".into()]),
            source_address: Some("10.0.0.0/8".into()),
            force_command: None,
        };
        let p = policy_from_form(&base, &form, &cert).unwrap();
        assert!(p.require_2fa && !p.allow_reveal && p.ai.approval == AiApproval::Always);
        assert_eq!(p.grace_days, 14);
        assert_eq!(p.ca.default_ttl_minutes, 30);
        assert_eq!(p.ca.max_ttl_minutes, 120);
        // The core's checks: default above max, grace too long, bad template.
        let bad = CertValues {
            ttl: Some(600),
            ..cert.clone()
        };
        assert!(policy_from_form(&base, &form, &bad).is_err());
        let mut long = form.clone();
        long.grace_days = "91".into();
        assert!(policy_from_form(&base, &long, &cert).is_err());
        let tmpl = CertValues {
            principals: Some(vec!["{nope}".into()]),
            ..cert
        };
        assert!(policy_from_form(&base, &form, &tmpl).is_err());
    }
}
