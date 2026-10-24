//! The SSH certificate authority of a team (Teams → Security) or of your
//! account (Settings → Account): one component for both
//! (docs-next/TEAMS-SECURITY.md §4).
//!
//! Everyone who can see it gets the fingerprint, the public key and the
//! `sshd_config` line. Owners and admins (or you, for your own) also:
//! create it (Ed25519 or ECDSA P-256), the install command for hosts with
//! its options (revoked keys list, principals file, refresh every 15
//! minutes), rotate it with an overlap, disable it with a typed
//! confirmation, the issued certificates and revocation by serial, by user
//! or by key.
//!
//! The install script is served only to managers (with their token), so
//! `curl … | sh` cannot fetch it from a host: the command copied here is the
//! script itself in a heredoc (public data only), and it can be saved too.

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde::Deserialize;
use serde_json::{Value, json};
use termoak_client::ApiClient;
use termoak_core::Id;
use termoak_core::access::{CaInfo, CaStatus, IssuedCertificate};

use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Whose CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaScope {
    /// A team (its id and name, the confirmation to disable it).
    Team(Id, String),
    /// Your own (personal vaults).
    Personal,
}

impl CaScope {
    /// Base of its routes.
    pub fn base(&self) -> String {
        match self {
            CaScope::Team(id, _) => format!("/api/v1/teams/{id}/ca"),
            CaScope::Personal => "/api/v1/me/ca".to_string(),
        }
    }

    /// What disabling it asks to type.
    pub fn confirm_word(&self) -> String {
        match self {
            CaScope::Team(_, name) => name.clone(),
            CaScope::Personal => "personal".to_string(),
        }
    }
}

/// `GET …/ca`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CaOverview {
    pub ca: Option<CaInfo>,
    #[serde(default)]
    pub previous: Vec<CaInfo>,
    #[serde(default)]
    pub sshd_config: Vec<String>,
    #[serde(default)]
    pub sshd_config_optional: Vec<String>,
    #[serde(default)]
    pub urls: CaUrls,
    #[serde(default)]
    pub can_manage: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct CaUrls {
    #[serde(default)]
    pub keys: String,
    #[serde(default)]
    pub krl: String,
    #[serde(default)]
    pub install: String,
}

/// Options of the install script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallOptions {
    pub krl: bool,
    pub principals: bool,
    pub refresh: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            krl: true,
            principals: false,
            refresh: true,
        }
    }
}

impl InstallOptions {
    /// Query of `install.sh`.
    pub fn query(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        format!(
            "krl={}&principals={}&refresh={}",
            b(self.krl),
            b(self.principals),
            b(self.refresh)
        )
    }
}

/// The command to paste on a host: the script in a heredoc, run as root.
pub fn install_command(script: &str) -> String {
    let body = script.trim_end_matches('\n');
    format!("sudo sh -s <<'TERMOAK_CA'\n{body}\nTERMOAK_CA\n")
}

/// The `sshd_config` lines the options add.
pub fn sshd_lines(o: &CaOverview, opts: InstallOptions) -> Vec<String> {
    let mut lines = o.sshd_config.clone();
    for l in &o.sshd_config_optional {
        let wanted = (l.starts_with("RevokedKeys") && opts.krl)
            || (l.starts_with("AuthorizedPrincipalsFile") && opts.principals);
        if wanted {
            lines.push(l.clone());
        }
    }
    lines
}

/// `?confirm=` value, URL-encoded.
fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub struct CaPanel {
    model: Entity<AppModel>,
    /// Account whose server (`None`: the current one).
    account: Option<Id>,
    scope: CaScope,
    overview: Option<CaOverview>,
    error: Option<String>,
    loading: bool,
    busy: bool,
    algorithm: ChoiceState<&'static str>,
    opts: InstallOptions,
    script: Option<String>,
    script_error: Option<String>,
    certs: Vec<IssuedCertificate>,
    certs_error: Option<String>,
    active_only: bool,
}

impl CaPanel {
    pub fn new(
        model: Entity<AppModel>,
        account: Option<Id>,
        scope: CaScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let algorithm = ui::choice_state(
            vec![
                Choice::new(t!("ca.algorithm.ed25519"), "ed25519"),
                Choice::new(t!("ca.algorithm.ecdsa"), "ecdsa-p256"),
            ],
            Some(&"ed25519"),
            window,
            cx,
        );
        let mut this = Self {
            model,
            account,
            scope,
            overview: None,
            error: None,
            loading: false,
            busy: false,
            algorithm,
            opts: InstallOptions::default(),
            script: None,
            script_error: None,
            certs: Vec::new(),
            certs_error: None,
            active_only: true,
        };
        this.load(window, cx);
        this
    }

    fn api(&self, cx: &App) -> Option<ApiClient> {
        self.model.read(cx).api_of(self.account)
    }

    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            self.error = Some(t!("vaults.not_signed_in").to_string());
            return;
        };
        let base = self.scope.base();
        self.loading = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { api.get::<CaOverview>(&base).await.map_err(api_error) },
            |this, res, window, cx| {
                this.loading = false;
                match res {
                    Ok(o) => {
                        let has = o.ca.is_some() || !o.previous.is_empty();
                        let manage = o.can_manage;
                        this.overview = Some(o);
                        this.error = None;
                        if has && manage {
                            this.load_script(window, cx);
                            this.load_certs(window, cx);
                        }
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn load_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let path = format!("{}/install.sh?{}", self.scope.base(), self.opts.query());
        runtime::run_in(
            cx,
            window,
            async move { api.get_text(&path).await.map_err(api_error) },
            |this, res, _, cx| {
                match res {
                    Ok(s) => {
                        this.script = Some(s);
                        this.script_error = None;
                    }
                    Err(e) => {
                        this.script = None;
                        this.script_error = Some(e);
                    }
                }
                cx.notify();
            },
        );
    }

    fn load_certs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let path = format!(
            "{}/certificates?limit=200{}",
            self.scope.base(),
            if self.active_only { "&active=1" } else { "" }
        );
        runtime::run_in(
            cx,
            window,
            async move {
                api.get::<Vec<IssuedCertificate>>(&path)
                    .await
                    .map_err(api_error)
            },
            |this, res, _, cx| {
                match res {
                    Ok(list) => {
                        this.certs = list;
                        this.certs_error = None;
                    }
                    Err(e) => this.certs_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    /// Sends a change and reloads (`ok`: the toast).
    fn change(
        &mut self,
        fut: impl std::future::Future<Output = Result<Value, String>> + Send + 'static,
        ok: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        cx.notify();
        runtime::run_in(cx, window, fut, move |this, res, window, cx| {
            this.busy = false;
            match res {
                Ok(_) => {
                    ui::success(window, cx, ok);
                    this.load(window, cx);
                    // Vaults learn whether team certificates work.
                    let account = this.account.or(this.model.read(cx).current_account);
                    if let Some(a) = account {
                        this.model.update(cx, |m, cx| m.sync_account(a, cx));
                    }
                }
                Err(e) => ui::error(window, cx, e),
            }
            cx.notify();
        });
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let algorithm = ui::chosen(&self.algorithm, cx).unwrap_or("ed25519");
        let base = self.scope.base();
        self.change(
            async move {
                api.post::<Value>(&base, &json!({"algorithm": algorithm}))
                    .await
                    .map_err(api_error)
            },
            t!("ca.created"),
            window,
            cx,
        );
    }

    fn rotate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hours = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("24")
                .default_value("24")
        });
        let weak = cx.entity().downgrade();
        let h2 = hours.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("ca.rotate.title"),
            t!("ca.rotate.ok"),
            440.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!("ca.rotate.message")))
                    .child(ui::field_with_hint(
                        t!("ca.rotate.overlap"),
                        Input::new(&hours),
                        t!("ca.rotate.overlap_hint"),
                        cx,
                    ))
                    .into_any_element()
            },
            move |window, cx| {
                let text = h2.read(cx).value().trim().to_string();
                let Some(overlap) = text.parse::<u32>().ok().filter(|h| (1..=720).contains(h))
                else {
                    ui::error(window, cx, t!("ca.rotate.overlap_error"));
                    return false;
                };
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |this, cx| {
                        let Some(api) = this.api(cx) else {
                            return;
                        };
                        let path = format!("{}/rotate", this.scope.base());
                        this.change(
                            async move {
                                api.post::<Value>(&path, &json!({"overlap_hours": overlap}))
                                    .await
                                    .map_err(api_error)
                            },
                            t!("ca.rotated"),
                            window,
                            cx,
                        );
                    });
                }
                true
            },
        );
    }

    fn disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let word = self.scope.confirm_word();
        let typed = cx.new(|cx| InputState::new(window, cx).placeholder(word.clone()));
        let weak = cx.entity().downgrade();
        let t2 = typed.clone();
        let w2 = word.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("ca.disable.title"),
            t!("ca.disable.ok"),
            460.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!("ca.disable.message")))
                    .child(ui::field(
                        t!("ca.disable.type", word = word.clone()),
                        Input::new(&typed),
                        cx,
                    ))
                    .into_any_element()
            },
            move |window, cx| {
                let text = t2.read(cx).value().trim().to_string();
                if text != w2 {
                    ui::error(window, cx, t!("ca.disable.mismatch"));
                    return false;
                }
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |this, cx| {
                        let Some(api) = this.api(cx) else {
                            return;
                        };
                        let path = format!("{}?confirm={}", this.scope.base(), encode(&text));
                        this.change(
                            async move { api.delete(&path).await.map_err(api_error) },
                            t!("ca.disabled"),
                            window,
                            cx,
                        );
                    });
                }
                true
            },
        );
    }

    fn revoke(&mut self, body: Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.api(cx) else {
            return;
        };
        let path = format!("{}/revoke", self.scope.base());
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { api.post::<Value>(&path, &body).await.map_err(api_error) },
            |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => {
                        let n = v["revoked"].as_array().map_or(0, Vec::len);
                        ui::success(window, cx, tn!("ca.revoked", n));
                        this.load_certs(window, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn revoke_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key =
            cx.new(|cx| InputState::new(window, cx).placeholder("ssh-ed25519 AAAA… user@laptop"));
        let reason = cx.new(|cx| InputState::new(window, cx).placeholder(t!("ca.revoke.reason")));
        let weak = cx.entity().downgrade();
        let (k2, r2) = (key.clone(), reason.clone());
        ui::open_form_dialog(
            window,
            cx,
            t!("ca.revoke_key.title"),
            t!("ca.revoke_key.ok"),
            520.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!("ca.revoke_key.message")))
                    .child(ui::field(t!("ca.revoke_key.key"), Input::new(&key), cx))
                    .child(ui::field(t!("ca.revoke.reason"), Input::new(&reason), cx))
                    .into_any_element()
            },
            move |window, cx| {
                let public_key = k2.read(cx).value().trim().to_string();
                if !public_key.starts_with("ssh-")
                    && !public_key.starts_with("ecdsa-")
                    && !public_key.starts_with("sk-")
                {
                    ui::error(window, cx, t!("ca.revoke_key.invalid"));
                    return false;
                }
                let reason = r2.read(cx).value().trim().to_string();
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |this, cx| {
                        this.revoke(
                            json!({"public_key": public_key, "reason": (!reason.is_empty()).then_some(reason)}),
                            window,
                            cx,
                        )
                    });
                }
                true
            },
        );
    }

    fn copy(&self, text: String, done: SharedString, window: &mut Window, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        ui::success(window, cx, done);
    }

    fn save_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(script) = self.script.clone() else {
            return;
        };
        let dir = directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("termoak-ca-install.sh"));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let res = std::fs::write(&path, script.as_bytes());
            let _ = cx.update(|window, cx| match res {
                Ok(()) => ui::success(
                    window,
                    cx,
                    t!("ca.install.saved", path = path.display().to_string()),
                ),
                Err(e) => ui::error(window, cx, e.to_string()),
            });
        })
        .detach();
    }

    // ----- Rendering -----

    fn code_block(
        &self,
        id: &'static str,
        text: String,
        copied: SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme();
        let t2 = text.clone();
        h_flex()
            .gap_2()
            .items_start()
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.copy(t2.clone(), copied.clone(), window, cx)
                    })),
            )
    }

    fn render_ca(&self, o: &CaOverview, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let manage = o.can_manage;
        let Some(ca) = &o.ca else {
            return v_flex()
                .gap_3()
                .child(div().text_sm().text_color(muted).child(match self.scope {
                    CaScope::Team(..) => t!("ca.none_team"),
                    CaScope::Personal => t!("ca.none_personal"),
                }))
                .when(manage, |this| {
                    this.child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(div().w(px(260.)).child(ui::field(
                                t!("ca.algorithm.label"),
                                Select::new(&self.algorithm),
                                cx,
                            )))
                            .child(
                                Button::new("ca-create")
                                    .primary()
                                    .icon(ui::icon(IconName::ShieldPlus))
                                    .label(t!("ca.create"))
                                    .loading(self.busy)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.create(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("ca.algorithm.hint")),
                    )
                })
                .into_any_element();
        };
        let lines = sshd_lines(o, self.opts);
        let retiring: Vec<&CaInfo> = o
            .previous
            .iter()
            .filter(|c| c.status == CaStatus::Retiring)
            .collect();
        let opts = self.opts;
        let toggle = |id: &'static str,
                      label: SharedString,
                      on: bool,
                      set: fn(&mut InstallOptions, bool),
                      cx: &mut Context<Self>| {
            Switch::new(id)
                .small()
                .label(label)
                .checked(on)
                .on_click(cx.listener(move |this, v: &bool, window, cx| {
                    set(&mut this.opts, *v);
                    this.script = None;
                    this.load_script(window, cx);
                    cx.notify();
                }))
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(ui::pill(t!("ca.status.active"), theme.success))
                    .child(div().text_sm().font_medium().child(ca.algorithm.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("ca.created_at", date = ui::format_ms(ca.created_at))),
                    ),
            )
            .child(ui::field(
                t!("ca.fingerprint"),
                self.code_block(
                    "ca-fp",
                    ca.fingerprint.clone(),
                    t!("ca.copied.fingerprint"),
                    cx,
                ),
                cx,
            ))
            .child(ui::field(
                t!("ca.public_key"),
                self.code_block(
                    "ca-pub",
                    ca.public_key.clone(),
                    t!("ca.copied.public_key"),
                    cx,
                ),
                cx,
            ))
            .children(retiring.iter().map(|c| {
                div().text_xs().text_color(theme.warning).child(t!(
                    "ca.retiring",
                    fingerprint = c.fingerprint.clone(),
                    date = c.retire_at.map(ui::format_ms).unwrap_or_default()
                ))
            }))
            .child(ui::field_with_hint(
                t!("ca.sshd_config"),
                self.code_block("ca-sshd", lines.join("\n"), t!("ca.copied.sshd"), cx),
                t!("ca.sshd_config_hint", url = o.urls.keys.clone()),
                cx,
            ))
            .when(manage, |this| {
                let script = self.script.clone();
                this.child(
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
                                .child(t!("ca.install.title")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("ca.install.hint")),
                        )
                        .child(
                            h_flex()
                                .gap_4()
                                .flex_wrap()
                                .child(toggle(
                                    "ca-opt-krl",
                                    t!("ca.install.krl"),
                                    opts.krl,
                                    |o, v| o.krl = v,
                                    cx,
                                ))
                                .child(toggle(
                                    "ca-opt-principals",
                                    t!("ca.install.principals"),
                                    opts.principals,
                                    |o, v| o.principals = v,
                                    cx,
                                ))
                                .child(toggle(
                                    "ca-opt-refresh",
                                    t!("ca.install.refresh"),
                                    opts.refresh,
                                    |o, v| o.refresh = v,
                                    cx,
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_family(ui::mono_family(cx))
                                .text_color(muted)
                                .child("sudo sh -s <<'TERMOAK_CA' … TERMOAK_CA"),
                        )
                        .when_some(self.script_error.clone(), |this, e| {
                            this.child(div().text_xs().text_color(theme.danger).child(e))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("ca-copy-install")
                                        .primary()
                                        .small()
                                        .icon(ui::icon(IconName::Copy))
                                        .label(t!("ca.install.copy"))
                                        .disabled(script.is_none())
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                if let Some(s) = &script {
                                                    this.copy(
                                                        install_command(s),
                                                        t!("ca.copied.install"),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            },
                                        )),
                                )
                                .child(
                                    Button::new("ca-save-install")
                                        .small()
                                        .icon(ui::icon(IconName::Download))
                                        .label(t!("ca.install.save"))
                                        .disabled(self.script.is_none())
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.save_script(window, cx)
                                            },
                                        )),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("ca-rotate")
                                .small()
                                .icon(ui::icon(IconName::RefreshCw))
                                .label(t!("ca.rotate.button"))
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.rotate(window, cx)
                                })),
                        )
                        .child(
                            Button::new("ca-disable")
                                .small()
                                .danger()
                                .icon(ui::icon(IconName::Ban))
                                .label(t!("ca.disable.button"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.disable(window, cx)
                                })),
                        ),
                )
                .child(self.render_certs(cx))
            })
            .into_any_element()
    }

    fn render_certs(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let now = termoak_core::time::now_ms();
        let weak = cx.entity().downgrade();
        let rows = self.certs.iter().enumerate().map(|(i, c)| {
            let revoked = c.revoked_at.is_some();
            let expired = c.valid_before <= now;
            let (serial, user) = (c.serial, c.user_id);
            let w = weak.clone();
            h_flex()
                .gap_2()
                .py_1()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .child(
                    div()
                        .w(px(48.))
                        .text_color(muted)
                        .child(format!("#{}", c.serial)),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(
                                    c.user_email
                                        .clone()
                                        .unwrap_or_else(|| c.user_id.to_string()),
                                ),
                        )
                        .child(div().text_color(muted).child(t!(
                            "ca.cert.detail",
                            principals = c.principals.join(", "),
                            purpose = c.purpose.clone()
                        ))),
                )
                .child(div().text_color(muted).child(if revoked {
                    t!("ca.cert.revoked")
                } else if expired {
                    t!("ca.cert.expired")
                } else {
                    t!("ca.cert.until", time = ui::format_ms(c.valid_before))
                }))
                .when(!revoked && !expired, |this| {
                    this.child(
                        Button::new(("ca-cert-menu", i))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::EllipsisVertical))
                            .dropdown_menu(move |menu, _, _| {
                                let (w1, w2) = (w.clone(), w.clone());
                                menu.item(
                                    PopupMenuItem::new(t!("ca.revoke.serial"))
                                        .icon(ui::icon(IconName::Ban))
                                        .on_click(move |_, window, cx| {
                                            if let Some(v) = w1.upgrade() {
                                                v.update(cx, |v, cx| {
                                                    v.revoke(json!({"serial": serial}), window, cx)
                                                });
                                            }
                                        }),
                                )
                                .item(
                                    PopupMenuItem::new(t!("ca.revoke.user"))
                                        .icon(ui::icon(IconName::UserX))
                                        .on_click(move |_, window, cx| {
                                            if let Some(v) = w2.upgrade() {
                                                v.update(cx, |v, cx| {
                                                    v.revoke(json!({"user_id": user}), window, cx)
                                                });
                                            }
                                        }),
                                )
                            }),
                    )
                })
        });
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_semibold()
                            .child(t!("ca.certs.title")),
                    )
                    .child(
                        Switch::new("ca-certs-active")
                            .small()
                            .label(t!("ca.certs.active_only"))
                            .checked(self.active_only)
                            .on_click(cx.listener(|this, v: &bool, window, cx| {
                                this.active_only = *v;
                                this.load_certs(window, cx);
                            })),
                    )
                    .child(
                        Button::new("ca-revoke-key")
                            .xsmall()
                            .icon(ui::icon(IconName::KeyRound))
                            .label(t!("ca.revoke_key.button"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.revoke_key(window, cx)
                            })),
                    ),
            )
            .when_some(self.certs_error.clone(), |this, e| {
                this.child(div().text_xs().text_color(theme.danger).child(e))
            })
            .when(
                self.certs.is_empty() && self.certs_error.is_none(),
                |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("ca.certs.empty")),
                    )
                },
            )
            .children(rows)
            .into_any_element()
    }
}

impl Render for CaPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let body = match (&self.overview, &self.error) {
            (_, Some(e)) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element(),
            (Some(o), None) => {
                let o = o.clone();
                self.render_ca(&o, cx)
            }
            (None, None) => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("common.loading"))
                .into_any_element(),
        };
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::ShieldCheck).size(px(16.)))
                    .child(div().font_semibold().child(t!("ca.title"))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match self.scope {
                        CaScope::Team(..) => t!("ca.hint_team"),
                        CaScope::Personal => t!("ca.hint_personal"),
                    }),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_command_wraps_the_script() {
        let cmd = install_command("#!/bin/sh\nset -eu\necho ok\n");
        assert!(cmd.starts_with("sudo sh -s <<'TERMOAK_CA'\n#!/bin/sh\n"));
        assert!(cmd.ends_with("echo ok\nTERMOAK_CA\n"));
        assert_eq!(
            InstallOptions::default().query(),
            "krl=1&principals=0&refresh=1"
        );
    }

    #[test]
    fn sshd_lines_follow_the_options() {
        let o = CaOverview {
            sshd_config: vec!["TrustedUserCAKeys /etc/ssh/termoak_user_ca.pub".into()],
            sshd_config_optional: vec![
                "RevokedKeys /etc/ssh/termoak_revoked.krl".into(),
                "AuthorizedPrincipalsFile /etc/ssh/termoak_principals/%u".into(),
            ],
            ..Default::default()
        };
        let all = InstallOptions {
            krl: true,
            principals: true,
            refresh: false,
        };
        assert_eq!(sshd_lines(&o, all).len(), 3);
        let none = InstallOptions {
            krl: false,
            principals: false,
            refresh: false,
        };
        assert_eq!(
            sshd_lines(&o, none),
            vec!["TrustedUserCAKeys /etc/ssh/termoak_user_ca.pub"]
        );
        assert_eq!(CaScope::Personal.base(), "/api/v1/me/ca");
        assert_eq!(CaScope::Personal.confirm_word(), "personal");
        assert_eq!(encode("Ops team"), "Ops+team");
    }
}
