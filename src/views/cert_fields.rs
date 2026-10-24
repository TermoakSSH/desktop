//! The certificate options of a team policy (its defaults) and of an access
//! policy (overrides for some hosts): principal templates with a preview,
//! TTLs, extensions, `source-address` and (access policies) `force-command`
//! (docs-next/TEAMS-SECURITY.md §4.5–4.7). Checked with the core's own
//! validators before saving.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window, div, prelude::FluentBuilder,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Disableable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::access::{CERT_EXTENSIONS, PrincipalContext, expand_principals};

use crate::ui;

/// Which policy the fields belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertFieldsKind {
    /// Team defaults: every field has a value.
    Team,
    /// Access policy overrides: empty fields keep the team's.
    Override,
}

/// The values typed, parsed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CertValues {
    pub principals: Option<Vec<String>>,
    pub ttl: Option<u32>,
    pub max_ttl: Option<u32>,
    pub extensions: Option<Vec<String>>,
    pub source_address: Option<String>,
    pub force_command: Option<String>,
}

/// Splits a comma or space separated list of principal templates.
pub fn parse_principals(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// What the templates give for an example member and host (the preview).
pub fn preview(templates: &[String], email: &str, user_id: Id, login: &str) -> Vec<String> {
    expand_principals(
        templates,
        &PrincipalContext {
            login: Some(login.to_string()),
            member: termoak_core::access::member_principal(email),
            email: email.to_string(),
            user_id,
        },
    )
}

pub struct CertFields {
    kind: CertFieldsKind,
    principals: Entity<InputState>,
    ttl: Entity<InputState>,
    max_ttl: Entity<InputState>,
    source: Entity<InputState>,
    force_command: Entity<InputState>,
    /// Chosen extensions (`None`: the team's, for overrides).
    extensions: Option<Vec<String>>,
    /// Example member for the preview.
    email: String,
    user_id: Id,
    _subs: Vec<Subscription>,
}

impl CertFields {
    pub fn new(
        kind: CertFieldsKind,
        initial: CertValues,
        email: String,
        user_id: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: SharedString,
                     value: String,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let inherit = || t!("cert_fields.inherit");
        let (p_ph, ttl_ph, ext) = match kind {
            CertFieldsKind::Team => ("{login}".into(), "60".into(), initial.extensions.clone()),
            CertFieldsKind::Override => (inherit(), inherit(), initial.extensions.clone()),
        };
        let principals = input(
            p_ph,
            initial.principals.clone().unwrap_or_default().join(", "),
            window,
            cx,
        );
        let ttl = input(
            ttl_ph,
            initial.ttl.map(|v| v.to_string()).unwrap_or_default(),
            window,
            cx,
        );
        let max_ttl = input(
            "480".into(),
            initial.max_ttl.map(|v| v.to_string()).unwrap_or_default(),
            window,
            cx,
        );
        let source = input(
            "10.0.0.0/8, 192.0.2.10".into(),
            initial.source_address.clone().unwrap_or_default(),
            window,
            cx,
        );
        let force_command = input(
            t!("cert_fields.force_command_placeholder"),
            initial.force_command.clone().unwrap_or_default(),
            window,
            cx,
        );
        let subs = vec![cx.subscribe(&principals, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        })];
        Self {
            kind,
            principals,
            ttl,
            max_ttl,
            source,
            force_command,
            extensions: ext,
            email,
            user_id,
            _subs: subs,
        }
    }

    /// The values, or the text of what is wrong.
    pub fn values(&self, cx: &gpui::App) -> Result<CertValues, String> {
        let text = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let number = |e: &Entity<InputState>| -> Result<Option<u32>, String> {
            let t = text(e);
            if t.is_empty() {
                return Ok(None);
            }
            t.parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| t!("cert_fields.error.number", value = t).to_string())
        };
        let principals = parse_principals(&text(&self.principals));
        let source = text(&self.source);
        let force = text(&self.force_command);
        Ok(CertValues {
            principals: (!principals.is_empty()).then_some(principals),
            ttl: number(&self.ttl)?,
            max_ttl: match self.kind {
                CertFieldsKind::Team => number(&self.max_ttl)?,
                CertFieldsKind::Override => None,
            },
            extensions: self.extensions.clone(),
            source_address: (!source.is_empty()).then_some(source),
            force_command: (!force.is_empty() && self.kind == CertFieldsKind::Override)
                .then_some(force),
        })
    }
}

impl Render for CertFields {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let templates = parse_principals(self.principals.read(cx).value().as_ref());
        let shown = if templates.is_empty() && self.kind == CertFieldsKind::Team {
            vec!["{login}".to_string()]
        } else {
            templates.clone()
        };
        let example = preview(&shown, &self.email, self.user_id, "deploy");
        let preview_text = if templates.is_empty() && self.kind == CertFieldsKind::Override {
            t!("cert_fields.preview_inherit")
        } else if example.is_empty() {
            t!("cert_fields.preview_none")
        } else {
            t!(
                "cert_fields.preview",
                email = self.email.clone(),
                principals = example.join(", ")
            )
        };
        let override_ext = self.kind == CertFieldsKind::Override;
        let ext_custom = self.extensions.is_some();
        let chosen = self.extensions.clone().unwrap_or_default();
        let ext_boxes =
            h_flex()
                .flex_wrap()
                .gap_3()
                .children(CERT_EXTENSIONS.iter().enumerate().map(|(i, name)| {
                    let on = if ext_custom {
                        chosen.iter().any(|e| e == name)
                    } else {
                        // The team's defaults (permit-pty, port and agent forwarding).
                        i < 3
                    };
                    Checkbox::new(("cert-ext", i))
                        .label(*name)
                        .checked(on)
                        .disabled(override_ext && !ext_custom)
                        .on_click(cx.listener(move |this, v: &bool, _, cx| {
                            let list = this.extensions.get_or_insert_with(Vec::new);
                            list.retain(|e| e != name);
                            if *v {
                                list.push(name.to_string());
                            }
                            cx.notify();
                        }))
                }));
        v_flex()
            .gap_3()
            .child(ui::field_with_hint(
                t!("cert_fields.principals"),
                Input::new(&self.principals),
                t!("cert_fields.principals_hint"),
                cx,
            ))
            .child(div().text_xs().text_color(muted).child(preview_text))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_1().child(ui::field(
                        match self.kind {
                            CertFieldsKind::Team => t!("cert_fields.default_ttl"),
                            CertFieldsKind::Override => t!("cert_fields.ttl"),
                        },
                        Input::new(&self.ttl),
                        cx,
                    )))
                    .when(self.kind == CertFieldsKind::Team, |this| {
                        this.child(div().flex_1().child(ui::field(
                            t!("cert_fields.max_ttl"),
                            Input::new(&self.max_ttl),
                            cx,
                        )))
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(t!("cert_fields.extensions")),
                    )
                    .when(override_ext, |this| {
                        this.child(
                            Checkbox::new("cert-ext-custom")
                                .label(t!("cert_fields.extensions_custom"))
                                .checked(ext_custom)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.extensions = v.then(|| {
                                        CERT_EXTENSIONS[..3].iter().map(|s| s.to_string()).collect()
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .child(ext_boxes),
            )
            .child(ui::field_with_hint(
                t!("cert_fields.source_address"),
                Input::new(&self.source),
                t!("cert_fields.source_address_hint"),
                cx,
            ))
            .when(self.kind == CertFieldsKind::Override, |this| {
                this.child(ui::field_with_hint(
                    t!("cert_fields.force_command"),
                    Input::new(&self.force_command),
                    t!("cert_fields.force_command_hint"),
                    cx,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_templates_and_preview() {
        assert_eq!(
            parse_principals("{login}, prod-{member}  deploy"),
            vec!["{login}", "prod-{member}", "deploy"]
        );
        let p = preview(
            &parse_principals("{login}, prod-{member}, {email}"),
            "Ana.Lopez+ops@example.com",
            Id::nil(),
            "deploy",
        );
        assert_eq!(
            p,
            vec!["deploy", "prod-ana.lopez_ops", "Ana.Lopez+ops@example.com"]
        );
        // Invalid results are dropped.
        assert!(preview(&["bad name".into()], "a@b.c", Id::nil(), "x").is_empty());
    }
}
