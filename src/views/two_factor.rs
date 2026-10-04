//! Two-step verification of the server account (Settings): status,
//! enabling it with the QR code for the authenticator app and the recovery
//! codes, and disabling it with the password and a code.

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement, Render,
    Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::clipboard::Clipboard;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde_json::{Value, json};

use crate::qr;
use crate::runtime;
use crate::state::{AppModel, ModelEvent, api_error};
use crate::ui::{self, IconName};

/// Step the user is at.
enum Phase {
    /// Looking at the status.
    Idle,
    /// Setting up the authenticator app.
    Setup { secret: String, otpauth_url: String },
    /// Just enabled: recovery codes (shown only now).
    Codes(Vec<String>),
}

pub struct TwoFactorPanel {
    model: Entity<AppModel>,
    /// (enabled, recovery codes left).
    status: Option<(bool, u64)>,
    loading: bool,
    busy: bool,
    error: Option<String>,
    phase: Phase,
    code: Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// Secret in groups of four, easier to type.
pub fn group_secret(secret: &str) -> String {
    secret
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

impl TwoFactorPanel {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let code = cx.new(|cx| InputState::new(window, cx).placeholder("123456"));
        let subs = vec![
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.phase = Phase::Idle;
                    this.refresh(window, cx);
                }
            }),
            cx.subscribe_in(&code, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.enable(window, cx);
                }
            }),
        ];
        let mut panel = Self {
            model,
            status: None,
            loading: false,
            busy: false,
            error: None,
            phase: Phase::Idle,
            code,
            _subs: subs,
        };
        panel.refresh(window, cx);
        panel
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            self.status = None;
            cx.notify();
            return;
        };
        self.loading = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { api.get::<Value>("/api/v1/me/2fa").await.map_err(api_error) },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(v) => {
                        this.status = Some((
                            v["enabled"].as_bool().unwrap_or(false),
                            v["recovery_codes_left"].as_u64().unwrap_or(0),
                        ));
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn start_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<Value>("/api/v1/me/2fa/setup", &json!({}))
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => {
                        let secret = v["secret"].as_str().unwrap_or("").to_string();
                        let otpauth_url = v["otpauth_url"].as_str().unwrap_or("").to_string();
                        this.code.update(cx, |i, cx| i.set_value("", window, cx));
                        this.phase = Phase::Setup {
                            secret,
                            otpauth_url,
                        };
                        ui::focus_later(&this.code, window, cx);
                    }
                    Err(e) => ui::error(window, cx, t!("two_factor.setup_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn enable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Setup { .. }) || self.busy {
            return;
        }
        let code: String = self
            .code
            .read(cx)
            .value()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
            ui::error(window, cx, t!("two_factor.enter_six_digits"));
            return;
        }
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<Value>("/api/v1/me/2fa/enable", &json!({"code": code}))
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => {
                        let codes: Vec<String> = v["recovery_codes"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|c| c.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        this.phase = Phase::Codes(codes);
                        ui::success(window, cx, t!("two_factor.enabled_toast"));
                        this.refresh(window, cx);
                        this.model.update(cx, |m, cx| m.refresh_me(cx));
                    }
                    Err(e) => {
                        this.code.update(cx, |i, cx| i.set_value("", window, cx));
                        ui::error(window, cx, t!("two_factor.enable_failed", error = e));
                    }
                }
                cx.notify();
            },
        );
    }

    fn open_disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("two_factor.password_placeholder"))
        });
        let code =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("two_factor.code_placeholder")));
        ui::focus_later(&password, window, cx);
        let (p2, c2) = (password.clone(), code.clone());
        let weak = cx.entity().downgrade();
        ui::open_form_dialog(
            window,
            cx,
            t!("two_factor.disable_title"),
            t!("two_factor.disable"),
            460.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("two_factor.disable_hint")),
                    )
                    .child(ui::field(
                        t!("two_factor.password"),
                        Input::new(&password).mask_toggle(),
                        cx,
                    ))
                    .child(ui::field(t!("two_factor.code"), Input::new(&code), cx))
                    .into_any_element()
            },
            move |window, cx| {
                let password = p2.read(cx).value().to_string();
                let code = c2.read(cx).value().trim().to_string();
                if password.is_empty() || code.is_empty() {
                    ui::error(window, cx, t!("two_factor.enter_password_and_code"));
                    return false;
                }
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| v.disable(password, code, window, cx));
                }
                true
            },
        );
    }

    fn disable(
        &mut self,
        password: String,
        code: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<Value>(
                    "/api/v1/me/2fa/disable",
                    &json!({"password": password, "code": code}),
                )
                .await
                .map_err(api_error)
            },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(_) => {
                        ui::success(window, cx, t!("two_factor.disabled_toast"));
                        this.refresh(window, cx);
                        this.model.update(cx, |m, cx| m.refresh_me(cx));
                    }
                    Err(e) => ui::error(window, cx, t!("two_factor.disable_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn render_setup(
        &self,
        secret: &str,
        otpauth_url: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let step = |n: &'static str, text: gpui::SharedString| {
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .size(px(20.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .text_xs()
                        .font_semibold()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(n),
                )
                .child(div().flex_1().min_w_0().text_sm().child(text))
        };
        let secret_owned = secret.to_string();
        h_flex()
            .gap_5()
            .items_start()
            .flex_wrap()
            .child(
                v_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .p_2()
                            .rounded(theme.radius)
                            .bg(gpui::white())
                            .child(qr::element(otpauth_url, 216.)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("two_factor.scan_with_app")),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w(px(260.))
                    .gap_3()
                    .child(step("1", t!("two_factor.step_scan")))
                    .child(step("2", t!("two_factor.step_manual")))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.background)
                                    .font_family(ui::mono_family(cx))
                                    .text_sm()
                                    .child(group_secret(secret)),
                            )
                            .child(
                                Clipboard::new("copy-secret")
                                    .small()
                                    .value(secret_owned)
                                    .tooltip(t!("two_factor.copy_secret")),
                            ),
                    )
                    .child(step("3", t!("two_factor.step_code")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().w(px(160.)).child(Input::new(&self.code)))
                            .child(
                                Button::new("2fa-confirm")
                                    .primary()
                                    .icon(ui::icon(IconName::ShieldCheck))
                                    .label(t!("two_factor.enable"))
                                    .loading(self.busy)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.enable(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("2fa-cancel")
                                    .ghost()
                                    .label(t!("common.cancel"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.phase = Phase::Idle;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_codes(&self, codes: &[String], cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let all = codes.join("\n");
        let mono = ui::mono_family(cx);
        let mut warning = theme.warning;
        warning.a = 0.12;
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .p_3()
                    .items_start()
                    .rounded(theme.radius)
                    .bg(warning)
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
                            .child(t!("two_factor.save_codes_warning")),
                    ),
            )
            .child(h_flex().flex_wrap().gap_2().children(codes.iter().map(|c| {
                div()
                    .w(px(150.))
                    .px_3()
                    .py_1p5()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .font_family(mono.clone())
                    .text_sm()
                    .text_center()
                    .child(c.clone())
            })))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("copy-codes")
                            .icon(ui::icon(IconName::Copy))
                            .label(t!("two_factor.copy_all"))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(all.clone()));
                                ui::success(window, cx, t!("two_factor.codes_copied"));
                            }),
                    )
                    .child(
                        Button::new("codes-done")
                            .primary()
                            .label(t!("two_factor.codes_saved"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.phase = Phase::Idle;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for TwoFactorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (enabled, left) = self.status.unwrap_or((false, 0));
        let known = self.status.is_some();
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let body: gpui::AnyElement = match &self.phase {
            Phase::Setup {
                secret,
                otpauth_url,
            } => {
                let (secret, url) = (secret.clone(), otpauth_url.clone());
                self.render_setup(&secret, &url, cx)
            }
            Phase::Codes(codes) => {
                let codes = codes.clone();
                self.render_codes(&codes, cx)
            }
            Phase::Idle => v_flex()
                .gap_3()
                .child(div().text_sm().text_color(muted).child(if enabled {
                    tn!("two_factor.enabled_hint", left)
                } else {
                    t!("two_factor.disabled_hint")
                }))
                .when_some(self.error.clone(), |this, e| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(danger)
                            .child(t!("two_factor.status_failed", error = e)),
                    )
                })
                .child(h_flex().gap_2().map(|this| {
                    if enabled {
                        this.child(
                            Button::new("2fa-disable")
                                .icon(ui::icon(IconName::ShieldOff))
                                .label(t!("two_factor.disable_ellipsis"))
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.open_disable(window, cx)
                                })),
                        )
                    } else {
                        this.child(
                            Button::new("2fa-enable")
                                .primary()
                                .icon(ui::icon(IconName::ShieldCheck))
                                .label(t!("two_factor.enable"))
                                .loading(self.busy || self.loading)
                                .disabled(!known)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.start_setup(window, cx)
                                })),
                        )
                    }
                }))
                .into_any_element(),
        };
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .pt_4()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::ShieldCheck)
                            .size(px(16.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(div().font_semibold().child(t!("two_factor.title")))
                    .when(known, |this| {
                        this.child(if enabled {
                            ui::pill(t!("two_factor.on"), theme.success)
                        } else {
                            ui::pill(t!("two_factor.off"), theme.muted_foreground)
                        })
                    }),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn secret_in_groups_of_four() {
        assert_eq!(
            super::group_secret("JBSWY3DPEHPK3PXPABC"),
            "JBSW Y3DP EHPK 3PXP ABC"
        );
        assert_eq!(super::group_secret(""), "");
    }
}
