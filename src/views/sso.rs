//! Single sign-on in the interface (0.7 phase 5b, docs-next/E2EE-SSO-AUDIT.md
//! §3 and §10): Teams → Single sign-on (owners and admins: the provider
//! with a preset, the redirect URI to register, the write-only client
//! secret, the domains with their TXT record and Verify, enforcement with
//! the owner's break glass, accounts created and added, "SSO counts as
//! 2FA"; members see a summary) and Settings → Security → Linked identities
//! (link through the system browser, unlink).

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde::Deserialize;
use serde_json::json;
use termoak_client::ApiClient;
use termoak_core::Id;
use termoak_core::sso::{SsoConfig, SsoPreset, TeamDomain};

use crate::runtime;
use crate::sso::{self, SsoForm};
use crate::state::{AppModel, ModelEvent, api_error};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// `GET /teams/{id}/sso`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TeamSsoData {
    #[serde(default)]
    pub config: Option<SsoConfig>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub domains: Vec<TeamDomain>,
}

fn muted(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn copy_row(id: impl Into<gpui::ElementId>, text: String, cx: &App) -> gpui::Div {
    let theme = cx.theme();
    let t2 = text.clone();
    h_flex()
        .gap_2()
        .items_center()
        .p_2()
        .rounded(theme.radius)
        .bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .font_family(ui::mono_family(cx))
                .child(text),
        )
        .child(
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::Copy))
                .tooltip(t!("common.copy"))
                .on_click(move |_: &ClickEvent, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(t2.clone()));
                    ui::success(window, cx, t!("sso.copied"));
                }),
        )
}

// ---------------------------------------------------------------------------
// Teams → Single sign-on
// ---------------------------------------------------------------------------

pub struct TeamSsoView {
    model: Entity<AppModel>,
    team: Id,
    team_name: String,
    manage: bool,
    data: Option<TeamSsoData>,
    error: Option<String>,
    form: SsoForm,
    preset: ChoiceState<SsoPreset>,
    issuer: Entity<InputState>,
    tenant: Entity<InputState>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    scopes: Entity<InputState>,
    email_claim: Entity<InputState>,
    new_domain: Entity<InputState>,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl TeamSsoView {
    pub fn new(
        model: Entity<AppModel>,
        team: Id,
        team_name: String,
        manage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let preset = ui::choice_state(
            sso::PRESETS
                .iter()
                .map(|p| Choice::new(sso::preset_label(*p), *p))
                .collect(),
            Some(&SsoPreset::Generic),
            window,
            cx,
        );
        let issuer = input(
            sso::preset_fields(SsoPreset::Generic).issuer_example.into(),
            window,
            cx,
        );
        let tenant = input("contoso.onmicrosoft.com".into(), window, cx);
        let client_id = input(
            t!("sso.field.client_id_placeholder").to_string(),
            window,
            cx,
        );
        let client_secret = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("sso.field.client_secret_placeholder"))
        });
        let scopes = input("groups".into(), window, cx);
        let email_claim = input("email".into(), window, cx);
        let new_domain = input("example.com".into(), window, cx);
        let subs = vec![
            cx.subscribe_in(
                &preset,
                window,
                |this, _, ev: &SelectEvent<Vec<Choice<SsoPreset>>>, window, cx| {
                    if let SelectEvent::Confirm(Some(p)) = ev {
                        this.set_preset(*p, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &new_domain,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.add_domain(window, cx);
                    }
                },
            ),
        ];
        let mut this = Self {
            model,
            team,
            team_name,
            manage,
            data: None,
            error: None,
            form: SsoForm::new_for(SsoPreset::Generic),
            preset,
            issuer,
            tenant,
            client_id,
            client_secret,
            scopes,
            email_claim,
            new_domain,
            busy: false,
            _subs: subs,
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
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<TeamSsoData>(&format!("/api/v1/teams/{team}/sso"))
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                match res {
                    Ok(d) => this.set_data(d, window, cx),
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn set_data(&mut self, d: TeamSsoData, window: &mut Window, cx: &mut Context<Self>) {
        let form = d
            .config
            .as_ref()
            .map(SsoForm::from_config)
            .unwrap_or_else(|| SsoForm::new_for(SsoPreset::Generic));
        let set =
            |i: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
                i.update(cx, |i, cx| i.set_value(v, window, cx))
            };
        set(&self.issuer, form.issuer.clone(), window, cx);
        set(&self.tenant, form.tenant.clone(), window, cx);
        set(&self.client_id, form.client_id.clone(), window, cx);
        set(&self.client_secret, String::new(), window, cx);
        set(&self.scopes, form.scopes.clone(), window, cx);
        set(&self.email_claim, form.email_claim.clone(), window, cx);
        self.preset
            .update(cx, |s, cx| s.set_selected_value(&form.preset, window, cx));
        self.form = form;
        self.data = Some(d);
        self.error = None;
    }

    fn set_preset(&mut self, p: SsoPreset, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.preset == p {
            return;
        }
        self.form.preset = p;
        let f = sso::preset_fields(p);
        self.form.require_email_verified = f.require_email_verified;
        self.issuer.update(cx, |i, cx| {
            i.set_placeholder(f.issuer_example, window, cx);
        });
        cx.notify();
    }

    fn read_form(&self, cx: &App) -> SsoForm {
        let v = |i: &Entity<InputState>| i.read(cx).value().to_string();
        SsoForm {
            preset: ui::chosen(&self.preset, cx).unwrap_or(self.form.preset),
            issuer: v(&self.issuer),
            tenant: v(&self.tenant),
            client_id: v(&self.client_id),
            client_secret: v(&self.client_secret),
            scopes: v(&self.scopes),
            email_claim: v(&self.email_claim),
            ..self.form.clone()
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let has_secret = self
            .data
            .as_ref()
            .and_then(|d| d.config.as_ref())
            .is_some_and(|c| c.has_client_secret);
        let form = self.read_form(cx);
        let config = match form.to_config(has_secret) {
            Ok(c) => c,
            Err(e) => return ui::error(window, cx, e),
        };
        let secret = Some(form.client_secret.trim().to_string()).filter(|s| !s.is_empty());
        let team = self.team;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                let mut body = json!({"config": config});
                if let Some(s) = secret {
                    body["client_secret"] = json!(s);
                }
                api.put::<TeamSsoData>(&format!("/api/v1/teams/{team}/sso"), &body)
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(d) => {
                        let on = d.config.as_ref().is_some_and(|c| c.enabled);
                        ui::success(
                            window,
                            cx,
                            if on {
                                t!("sso.saved_on")
                            } else {
                                t!("sso.saved")
                            },
                        );
                        this.set_data(d, window, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("sso.remove_title"),
            t!("sso.remove_message", team = self.team_name.clone()),
            t!("sso.remove"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                v.update(cx, |this, cx| {
                    let Some(api) = this.api(cx) else {
                        return;
                    };
                    let team = this.team;
                    runtime::run_in(
                        cx,
                        window,
                        async move {
                            api.delete(&format!("/api/v1/teams/{team}/sso"))
                                .await
                                .map_err(api_error)
                        },
                        |this, res, window, cx| match res {
                            Ok(_) => {
                                ui::success(window, cx, t!("sso.removed"));
                                this.load(window, cx);
                            }
                            Err(e) => ui::error(window, cx, e),
                        },
                    );
                });
            },
        );
    }

    fn add_domain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let domain = self.new_domain.read(cx).value().trim().to_lowercase();
        if domain.is_empty() {
            return;
        }
        if termoak_core::sso::normalize_domain(&domain).is_err() {
            return ui::error(window, cx, t!("sso.domain.invalid"));
        }
        let Some(api) = self.api(cx) else {
            return;
        };
        let team = self.team;
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<TeamDomain>(
                    &format!("/api/v1/teams/{team}/domains"),
                    &json!({"domain": domain}),
                )
                .await
                .map_err(api_error)
            },
            |this, res, window, cx| match res {
                Ok(_) => {
                    this.new_domain
                        .update(cx, |i, cx| i.set_value("", window, cx));
                    ui::success(window, cx, t!("sso.domain.added"));
                    this.load(window, cx);
                }
                Err(e) => ui::error(window, cx, e),
            },
        );
    }

    fn verify_domain(&mut self, domain: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let team = self.team;
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<TeamDomain>(
                    &format!("/api/v1/teams/{team}/domains/{domain}/verify"),
                    &json!({}),
                )
                .await
                .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(d) if d.verified_at.is_some() => ui::success(
                        window,
                        cx,
                        t!("sso.domain.verified_toast", domain = d.domain),
                    ),
                    Ok(d) => ui::error(
                        window,
                        cx,
                        t!(
                            "sso.domain.not_yet",
                            domain = d.domain,
                            error = d.last_error.unwrap_or_default()
                        ),
                    ),
                    Err(e) => ui::error(window, cx, e),
                }
                this.load(window, cx);
                cx.notify();
            },
        );
    }

    fn remove_domain(&mut self, domain: String, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let last_verified = self.data.as_ref().is_some_and(|d| {
            d.domains.iter().filter(|x| x.verified_at.is_some()).count() == 1
                && d.domains
                    .iter()
                    .any(|x| x.domain == domain && x.verified_at.is_some())
        });
        ui::confirm(
            window,
            cx,
            t!("sso.domain.remove_title", domain = domain.clone()),
            if last_verified {
                t!("sso.domain.remove_last")
            } else {
                t!("sso.domain.remove_message")
            },
            t!("sso.domain.remove"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                let domain = domain.clone();
                v.update(cx, |this, cx| {
                    let Some(api) = this.api(cx) else {
                        return;
                    };
                    let team = this.team;
                    runtime::run_in(
                        cx,
                        window,
                        async move {
                            api.delete(&format!("/api/v1/teams/{team}/domains/{domain}"))
                                .await
                                .map_err(api_error)
                        },
                        |this, res, window, cx| match res {
                            Ok(_) => this.load(window, cx),
                            Err(e) => ui::error(window, cx, e),
                        },
                    );
                });
            },
        );
    }

    fn switch(
        &self,
        id: &'static str,
        label: SharedString,
        hint: SharedString,
        on: bool,
        disabled: bool,
        set: fn(&mut SsoForm, bool),
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        v_flex()
            .gap_0p5()
            .child(
                Switch::new(id)
                    .label(label)
                    .checked(on)
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, v: &bool, _, cx| {
                        set(&mut this.form, *v);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
    }

    fn render_domains(&self, data: &TeamSsoData, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mut col = v_flex()
            .gap_2()
            .child(div().font_semibold().child(t!("sso.domains.title")))
            .child(muted(t!("sso.domains.hint"), cx));
        if data.domains.is_empty() {
            col = col.child(muted(t!("sso.domains.none"), cx));
        }
        for (i, d) in data.domains.iter().enumerate() {
            let (_, ok) = sso::domain_state(d);
            let state = if ok {
                sso::domain_state(d).0
            } else {
                t!("sso.domain.pending").to_string()
            };
            let last_error = d.last_error.clone().filter(|e| !ok && !e.is_empty());
            let dom = d.domain.clone();
            let dom2 = d.domain.clone();
            col = col.child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().font_medium().child(d.domain.clone()))
                            .child(ui::pill(
                                state,
                                if ok { theme.success } else { theme.warning },
                            ))
                            .child(div().flex_1())
                            .when(!ok && self.manage, |this| {
                                this.child(
                                    Button::new(("dom-verify", i))
                                        .small()
                                        .primary()
                                        .label(t!("sso.domain.verify"))
                                        .loading(self.busy)
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.verify_domain(dom.clone(), window, cx)
                                            },
                                        )),
                                )
                            })
                            .when(self.manage, |this| {
                                this.child(
                                    Button::new(("dom-remove", i))
                                        .small()
                                        .ghost()
                                        .icon(ui::icon(IconName::Trash))
                                        .tooltip(t!("sso.domain.remove"))
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.remove_domain(dom2.clone(), window, cx)
                                            },
                                        )),
                                )
                            }),
                    )
                    .when_some(last_error, |this, e| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.warning)
                                .child(t!("sso.domain.failed", error = e)),
                        )
                    })
                    .when(!ok && !d.record_value.is_empty(), |this| {
                        this.child(muted(t!("sso.domain.txt_hint"), cx))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(72.))
                                            .flex_shrink_0()
                                            .text_xs()
                                            .child(t!("sso.domain.txt_name")),
                                    )
                                    .child(div().flex_1().min_w_0().child(copy_row(
                                        ("dom-name", i),
                                        d.record_name.clone(),
                                        cx,
                                    ))),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(72.))
                                            .flex_shrink_0()
                                            .text_xs()
                                            .child(t!("sso.domain.txt_value")),
                                    )
                                    .child(div().flex_1().min_w_0().child(copy_row(
                                        ("dom-value", i),
                                        d.record_value.clone(),
                                        cx,
                                    ))),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(72.))
                                            .flex_shrink_0()
                                            .text_xs()
                                            .child(t!("sso.domain.txt_zone")),
                                    )
                                    .child(div().flex_1().min_w_0().child(copy_row(
                                        ("dom-zone", i),
                                        sso::txt_zone_line(d),
                                        cx,
                                    ))),
                            )
                    }),
            );
        }
        if self.manage {
            col = col.child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(280.)).child(Input::new(&self.new_domain)))
                    .child(
                        Button::new("dom-add")
                            .icon(ui::icon(IconName::Plus))
                            .label(t!("sso.domain.add"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.add_domain(window, cx)
                            })),
                    ),
            );
        }
        col.into_any_element()
    }
}

impl Render for TeamSsoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(data) = self.data.clone() else {
            return match &self.error {
                Some(e) => div().text_sm().text_color(theme.danger).child(e.clone()),
                None => h_flex()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(muted(t!("common.loading"), cx)),
            }
            .into_any_element();
        };
        let cfg = data.config.clone();
        let state_pill = match &cfg {
            Some(c) if c.enabled && c.enforce => ui::pill(t!("sso.state.enforced"), theme.primary),
            Some(c) if c.enabled => ui::pill(t!("sso.state.on"), theme.success),
            Some(_) => ui::pill(t!("sso.state.off"), theme.muted_foreground),
            None => ui::pill(t!("sso.state.not_set_up"), theme.muted_foreground),
        };
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(ui::icon(IconName::LogIn).text_color(theme.primary))
            .child(div().font_semibold().child(t!("sso.title")))
            .child(state_pill);
        if !self.manage {
            let summary = match &cfg {
                Some(c) if c.enabled => t!(
                    "sso.member_summary",
                    provider = sso::preset_label(c.preset),
                    enforced = if c.enforce {
                        t!("sso.member_enforced")
                    } else {
                        t!("sso.member_optional")
                    }
                ),
                _ => t!("sso.member_none"),
            };
            return v_flex()
                .gap_3()
                .child(header)
                .child(muted(summary, cx))
                .children(
                    data.domains
                        .iter()
                        .map(|d| muted(format!("• {}", d.domain), cx)),
                )
                .into_any_element();
        }
        let preset = ui::chosen(&self.preset, cx).unwrap_or(self.form.preset);
        let fields = sso::preset_fields(preset);
        let has_secret = cfg.as_ref().is_some_and(|c| c.has_client_secret);
        let can_enforce = sso::can_enforce(&data.domains);
        let form = self.form.clone();
        let provider = v_flex()
            .gap_3()
            .child(div().font_semibold().child(t!("sso.provider.title")))
            .child(div().w(px(340.)).child(ui::field(
                t!("sso.field.preset"),
                Select::new(&self.preset),
                cx,
            )))
            .child(muted(t!(fields.help_key), cx))
            .child(match &data.redirect_uri {
                Some(uri) => ui::field(
                    t!("sso.field.redirect_uri"),
                    copy_row("sso-redirect", uri.clone(), cx),
                    cx,
                ),
                None => div()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(t!("sso.no_public_url")),
            })
            .when(fields.tenant, |this| {
                this.child(ui::field(
                    t!("sso.field.tenant"),
                    Input::new(&self.tenant),
                    cx,
                ))
            })
            .when(fields.issuer, |this| {
                this.child(ui::field(
                    t!("sso.field.issuer"),
                    Input::new(&self.issuer),
                    cx,
                ))
            })
            .child(ui::field(
                t!("sso.field.client_id"),
                Input::new(&self.client_id),
                cx,
            ))
            .child(ui::field_with_hint(
                t!("sso.field.client_secret"),
                Input::new(&self.client_secret),
                if has_secret {
                    t!("sso.field.client_secret_stored")
                } else {
                    t!("sso.field.client_secret_hint")
                },
                cx,
            ))
            .child(ui::field_with_hint(
                t!("sso.field.scopes"),
                Input::new(&self.scopes),
                t!("sso.field.scopes_hint"),
                cx,
            ))
            .child(ui::field_with_hint(
                t!("sso.field.email_claim"),
                Input::new(&self.email_claim),
                t!("sso.field.email_claim_hint"),
                cx,
            ))
            .child(self.switch(
                "sso-email-verified",
                t!("sso.rule.email_verified"),
                t!("sso.rule.email_verified_hint"),
                form.require_email_verified,
                false,
                |f, v| f.require_email_verified = v,
                cx,
            ));
        let rules = v_flex()
            .gap_3()
            .child(div().font_semibold().child(t!("sso.rules.title")))
            .child(self.switch(
                "sso-enabled",
                t!("sso.rule.enabled"),
                t!("sso.rule.enabled_hint"),
                form.enabled,
                false,
                |f, v| f.enabled = v,
                cx,
            ))
            .child(self.switch(
                "sso-enforce",
                t!("sso.rule.enforce"),
                if can_enforce {
                    t!("sso.rule.enforce_hint")
                } else {
                    t!("sso.rule.enforce_needs_domain")
                },
                form.enforce,
                !can_enforce && !form.enforce,
                |f, v| f.enforce = v,
                cx,
            ))
            .when(form.enforce, |this| {
                this.child(self.switch(
                    "sso-owner",
                    t!("sso.rule.owner_password"),
                    t!("sso.rule.owner_password_hint"),
                    form.owner_password_allowed,
                    false,
                    |f, v| f.owner_password_allowed = v,
                    cx,
                ))
            })
            .child(self.switch(
                "sso-create",
                t!("sso.rule.auto_create"),
                t!("sso.rule.auto_create_hint"),
                form.auto_create,
                false,
                |f, v| f.auto_create = v,
                cx,
            ))
            .child(self.switch(
                "sso-join",
                t!("sso.rule.auto_join"),
                t!("sso.rule.auto_join_hint"),
                form.auto_join,
                false,
                |f, v| f.auto_join = v,
                cx,
            ))
            .child(self.switch(
                "sso-2fa",
                t!("sso.rule.satisfies_2fa"),
                t!("sso.rule.satisfies_2fa_hint"),
                form.satisfies_2fa,
                false,
                |f, v| f.satisfies_2fa = v,
                cx,
            ));
        v_flex()
            .gap_5()
            .child(header)
            .child(muted(t!("sso.intro"), cx))
            .child(provider)
            .child(self.render_domains(&data, cx))
            .child(rules)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(cfg.is_some(), |this| {
                        this.child(
                            Button::new("sso-remove")
                                .ghost()
                                .danger()
                                .label(t!("sso.remove"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.remove(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("sso-save")
                            .primary()
                            .icon(ui::icon(IconName::Save))
                            .label(t!("sso.save"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Settings → Security → Linked identities
// ---------------------------------------------------------------------------

/// `GET /me/sso`.
#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub issuer: String,
    pub team_id: Id,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub last_login_at: Option<i64>,
}

pub struct LinkedIdentities {
    model: Entity<AppModel>,
    identities: Vec<Identity>,
    /// Teams of yours with SSO on: (id, name).
    sso_teams: Vec<(Id, String)>,
    loaded: bool,
    error: Option<String>,
    linking: Option<Id>,
    _subs: Vec<Subscription>,
}

impl LinkedIdentities {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.load(window, cx);
                }
            }),
        ];
        let mut this = Self {
            model,
            identities: Vec::new(),
            sso_teams: Vec::new(),
            loaded: false,
            error: None,
            linking: None,
            _subs: subs,
        };
        this.load(window, cx);
        this
    }

    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (api, teams) = {
            let m = self.model.read(cx);
            let sso = m.current_account.is_some_and(|a| m.sso_supported(a));
            if !sso {
                return;
            }
            (
                m.api.clone(),
                m.teams
                    .iter()
                    .map(|t| (t.id, t.name.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        let Some(api) = api else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move {
                let ids: Vec<Identity> = api.get("/api/v1/me/sso").await.map_err(api_error)?;
                let mut on = Vec::new();
                for (id, name) in teams {
                    if let Ok(d) = api
                        .get::<TeamSsoData>(&format!("/api/v1/teams/{id}/sso"))
                        .await
                        && d.config.is_some_and(|c| c.enabled)
                    {
                        on.push((id, name));
                    }
                }
                Ok::<_, String>((ids, on))
            },
            |this, res, _, cx| {
                this.loaded = true;
                match res {
                    Ok((ids, on)) => {
                        this.identities = ids;
                        this.sso_teams = on;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn link(&mut self, team: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        self.linking = Some(team);
        cx.notify();
        let device = termoak_client::api::device_name();
        runtime::run_in(
            cx,
            window,
            async move { sso::link(api, team, device).await.map_err(api_error) },
            move |this, res, window, cx| match res {
                Ok((url, pending, lb)) => {
                    cx.open_url(&url);
                    ui::notify(
                        window,
                        cx,
                        crate::state::ToastKind::Info,
                        t!("sso.browser_opened"),
                    );
                    runtime::run_in(
                        cx,
                        window,
                        async move { sso::link_wait(pending, lb).await.map_err(api_error) },
                        |this, res, window, cx| {
                            this.linking = None;
                            match res {
                                Ok(()) => {
                                    ui::success(window, cx, t!("sso.linked"));
                                    this.load(window, cx);
                                }
                                Err(e) => ui::error(window, cx, e),
                            }
                            cx.notify();
                        },
                    );
                }
                Err(e) => {
                    this.linking = None;
                    ui::error(window, cx, e);
                    cx.notify();
                }
            },
        );
    }

    fn unlink(&mut self, team: Id, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("sso.unlink_title"),
            t!("sso.unlink_message", team = name),
            t!("sso.unlink"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                v.update(cx, |this, cx| {
                    let Some(api) = this.model.read(cx).api.clone() else {
                        return;
                    };
                    runtime::run_in(
                        cx,
                        window,
                        async move {
                            api.delete(&format!("/api/v1/me/sso/{team}"))
                                .await
                                .map_err(api_error)
                        },
                        |this, res, window, cx| match res {
                            Ok(_) => {
                                ui::success(window, cx, t!("sso.unlinked"));
                                this.load(window, cx);
                            }
                            Err(e) => ui::error(window, cx, e),
                        },
                    );
                });
            },
        );
    }
}

impl Render for LinkedIdentities {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (sso, sso_only) = {
            let m = self.model.read(cx);
            (
                m.current_account.is_some_and(|a| m.sso_supported(a)),
                m.me.as_ref().is_some_and(|u| u.sso_only),
            )
        };
        if !sso {
            return div().into_any_element();
        }
        let team_name = |id: Id| {
            self.model
                .read(cx)
                .teams
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| id.to_string())
        };
        let mut col = v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::Link).text_color(theme.primary))
                    .child(div().font_semibold().child(t!("sso.identities.title"))),
            )
            .child(muted(t!("sso.identities.hint"), cx));
        if let Some(e) = &self.error {
            col = col.child(div().text_sm().text_color(theme.danger).child(e.clone()));
        }
        if self.loaded && self.identities.is_empty() {
            col = col.child(muted(t!("sso.identities.none"), cx));
        }
        for (i, ident) in self.identities.iter().enumerate() {
            let name = team_name(ident.team_id);
            let (team, n2) = (ident.team_id, name.clone());
            col = col.child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .child(format!("{name} · {}", ident.email)),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                match ident.last_login_at {
                                    Some(ms) => t!(
                                        "sso.identities.last_login",
                                        issuer = ident.issuer.clone(),
                                        date = ui::format_ms(ms)
                                    ),
                                    None => SharedString::from(ident.issuer.clone()),
                                },
                            )),
                    )
                    .child(
                        Button::new(("sso-unlink", i))
                            .small()
                            .ghost()
                            .label(t!("sso.unlink"))
                            .disabled(sso_only)
                            .tooltip(if sso_only {
                                t!("error.sso_only_account")
                            } else {
                                t!("sso.unlink")
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.unlink(team, n2.clone(), window, cx)
                            })),
                    ),
            );
        }
        let linkable: Vec<(Id, String)> = self
            .sso_teams
            .iter()
            .filter(|(t, _)| !self.identities.iter().any(|i| i.team_id == *t))
            .cloned()
            .collect();
        col = col.child(h_flex().gap_2().flex_wrap().children(
            linkable.into_iter().enumerate().map(|(i, (team, name))| {
                Button::new(("sso-link", i))
                    .icon(ui::icon(IconName::LogIn))
                    .label(t!("sso.link_with", team = name))
                    .loading(self.linking == Some(team))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.link(team, window, cx)
                    }))
            }),
        ));
        col.into_any_element()
    }
}
